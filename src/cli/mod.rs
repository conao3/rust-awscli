//! The command line: global options, the service / operation command
//! tables derived from the models, argument parsing with the AWS CLI's
//! messages, parameter values (shorthand, JSON, `file://`), and parameter
//! validation (`awscli/clidriver.py`, `awscli/argparser.py`,
//! `awscli/arguments.py`, `awscli/argprocess.py`, `awscli/shorthand.py`,
//! `awscli/paramfile.py`, the generic customizations, and
//! `botocore/validate.py`).

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
}

/// Split the global options out of `args` (`args[0]` is the first argument
/// after the program name). Errors carry the exact stderr text and exit
/// code of the AWS CLI.
pub fn parse_globals(args: &[String]) -> Result<(GlobalArgs, Vec<String>), CliError> {
    let _ = args;
    todo!("cli::parse_globals")
}

/// Parse the remaining arguments into a command.
pub fn parse_command(
    rest: &[String],
    globals: &GlobalArgs,
    config: &dyn ConfigLookup,
) -> Result<Command, CliError> {
    let _ = (rest, globals, config);
    todo!("cli::parse_command")
}
