//! Errors that end a command, with the AWS CLI v2 exit code they map to.

use std::fmt;

/// AWS CLI v2 exit codes (`awscli/topics/return-codes.rst`,
/// `awscli/argparser.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    S3TransferFailed = 1,
    S3Skipped = 2,
    Interrupted = 130,
    /// Command syntax, unknown parameter, invalid parameter value, or a
    /// parameter validation error.
    Usage = 252,
    /// Invalid environment or configuration (e.g. missing credentials).
    Config = 253,
    /// The service returned an error.
    Service = 254,
    General = 255,
}

/// An error that stops the CLI. `message` is written to stderr exactly as
/// given (the formatting rules of `awscli/errorformat.py` and
/// `awscli/errorhandler.py` are applied by whoever builds it).
#[derive(Debug)]
pub struct CliError {
    pub code: ExitCode,
    pub message: String,
}

impl CliError {
    pub fn new(code: ExitCode, message: impl Into<String>) -> CliError {
        CliError {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
