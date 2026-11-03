//! `deploy` customizations (`awscli/customizations/codedeploy/`): the
//! revision location arguments of three operations, and the `push`,
//! `register`, `deregister`, `install`, and `uninstall` commands.

mod ops;

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "create-deployment" | "get-application-revision" | "register-application-revision" => {
            ops::run(ctx, name, args)
        }
        _ => commands(ctx, name, args),
    }
}

fn commands(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let _ = (ctx, name, args);
    Err(crate::commands::not_implemented())
}
