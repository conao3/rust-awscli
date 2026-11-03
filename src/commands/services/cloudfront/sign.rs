//! `cloudfront sign` / `sign-cookies` (`awscli/customizations/cloudfront/sign.py`).

use crate::driver::Context;
use crate::error::{CliError, ExitCode};

pub(super) fn sign(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let _ = (ctx, args);
    Err(crate::commands::not_implemented())
}

pub(super) fn sign_cookies(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let _ = (ctx, args);
    Err(crate::commands::not_implemented())
}
