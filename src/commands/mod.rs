//! Commands the AWS CLI implements in its customizations rather than from
//! the service models: high-level commands (`s3`, `configure`, `sso`,
//! `login`, ...), commands a customization adds to a service, and
//! operations whose arguments a customization reshapes.

pub(crate) mod configure;
mod history;
mod login;
mod logout;
pub(crate) mod s3;
pub(crate) mod services;
pub(crate) mod sso;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

/// Run the custom command at `path` with `args` (the arguments after the
/// command path).
pub(crate) fn run(ctx: &Context, path: &[String], args: &[String]) -> Result<ExitCode, CliError> {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["configure"] => configure::run(ctx, args),
        ["history"] => history::run(ctx, args),
        ["sso", "login"] => sso::login(ctx, args),
        ["sso", "logout"] => sso::logout(ctx, args),
        ["login"] => login::run(ctx, args),
        ["logout"] => logout::run(ctx, args),
        ["s3"] => s3::run(ctx, args),
        [service, name] if services::handles(service) => services::run(ctx, service, name, args),
        _ => Err(not_implemented()),
    }
}

pub(crate) fn not_implemented() -> CliError {
    CliError::new(
        ExitCode::General,
        "\nawsr: [ERROR]: command not implemented yet\n",
    )
}
