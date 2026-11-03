//! The `lambda` customizations.

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let _ = (ctx, name, args);
    Err(crate::commands::not_implemented())
}
