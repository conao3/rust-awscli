//! `cloudformation package` and `cloudformation deploy`
//! (`awscli/customizations/cloudformation/`).

mod deploy;
mod package;
mod uploader;
mod yamlhelper;
mod zip;

use super::basic;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "package" => package::run(ctx, args),
        "deploy" => deploy::run(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// What the commands raise.
#[derive(Debug)]
pub(super) enum CfnError {
    /// An exception handled by `GeneralExceptionHandler`: `str(ex)`.
    General(String),
    /// `ParamValidationError(message)`.
    Param(String),
    /// An error already rendered (a `ClientError`, a parse failure).
    Cli(CliError),
}

impl From<CliError> for CfnError {
    fn from(e: CliError) -> CfnError {
        CfnError::Cli(e)
    }
}

impl CfnError {
    pub(super) fn render(self, ctx: &Context) -> CliError {
        match self {
            CfnError::General(m) => ctx.general_error(&anyhow::anyhow!("{m}")),
            CfnError::Param(m) => basic::param_validation(ctx, &m),
            CfnError::Cli(e) => e,
        }
    }

    /// `str(ex)` for wrapping in `ExportFailedError`.
    pub(super) fn text(&self) -> String {
        match self {
            CfnError::General(m) | CfnError::Param(m) => m.clone(),
            CfnError::Cli(e) => e
                .message
                .trim()
                .strip_prefix("aws: [ERROR]: ")
                .unwrap_or(e.message.trim())
                .to_string(),
        }
    }
}

/// `str(ClientError)`.
pub(super) fn client_error_text(info: &crate::output::errors::ClientErrorInfo) -> String {
    let retry = match info.max_retries_reached {
        Some(n) => format!(" (reached max retries: {n})"),
        None => String::new(),
    };
    format!(
        "An error occurred ({}) when calling the {} operation{retry}: {}",
        info.code, info.operation_name, info.message
    )
}
