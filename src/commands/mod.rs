//! Commands the AWS CLI implements in its customizations rather than from
//! the service models: high-level commands (`s3`, `configure`, `sso`,
//! `login`, ...), commands a customization adds to a service, and
//! operations whose arguments a customization reshapes.

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

/// Run the custom command at `path` with `args` (the arguments after the
/// command path).
pub(crate) fn run(ctx: &Context, path: &[String], args: &[String]) -> Result<ExitCode, CliError> {
    let _ = (ctx, path, args);
    Err(not_implemented())
}

pub(crate) fn not_implemented() -> CliError {
    CliError::new(
        ExitCode::General,
        "\naws: [ERROR]: awsr: command not implemented yet\n",
    )
}
