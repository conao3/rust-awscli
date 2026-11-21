//! `aws agent-registry` (`awscli/customizations/agentregistry/`): the
//! `list-skills`, `install-skill`, `uninstall-skill` and `sign-out`
//! commands, the `--authorizer JWT` sign-in, and the handlers on the
//! generated data plane commands (`initialize`).

mod browserflow;
mod cache;
mod casefold;
mod constants;
mod dataplane;
mod frontmatter;
mod install_skill;
mod list_skills;
mod providers;
mod sign_out;
mod uninstall_skill;
mod urlreq;
mod utils;
mod zipread;

use std::sync::atomic::{AtomicBool, Ordering};

use super::basic;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::session::sign::SignatureVersion;
use crate::session::{ErrorKind, SessionError};

/// What a command of the customization raises.
#[derive(Debug)]
pub(crate) enum Error {
    /// `AgentRegistryJWTError` (and its subclass `_Refused`).
    Jwt(String),
    /// `ParamValidationError` (`awscli.customizations.exceptions`).
    Param(String),
    /// Another exception, by `str(e)`, which the driver reports as is.
    Other(String),
    /// An error already rendered (a `ClientError` of a call, a botocore
    /// error).
    Cli(CliError),
}

impl Error {
    /// `str(e)`.
    pub(crate) fn message(&self) -> String {
        match self {
            Error::Jwt(m) | Error::Param(m) | Error::Other(m) => m.clone(),
            Error::Cli(e) => e.message.clone(),
        }
    }

    fn render(self, ctx: &Context) -> CliError {
        match self {
            Error::Jwt(m) | Error::Other(m) => ctx.general_error(&anyhow::anyhow!(m)),
            Error::Param(m) => basic::param_validation(ctx, &m),
            Error::Cli(e) => e,
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn run(
    ctx: &Context,
    name: &str,
    args: &[String],
) -> std::result::Result<ExitCode, CliError> {
    let result = match name {
        "list-skills" => list_skills::run(ctx, args),
        "install-skill" => install_skill::run(ctx, args),
        "uninstall-skill" => uninstall_skill::run(ctx, args),
        "sign-out" => sign_out::run(ctx, args),
        _ => return Err(crate::commands::not_implemented()),
    };
    result.map_err(|e| e.render(ctx))
}

/// Whether `before-sign.agent-registry` refuses a bearer-signed request
/// (`arm_unverified_tls_check` registered `refuse_bearer_signature`).
static UNVERIFIED_TLS_CHECK: AtomicBool = AtomicBool::new(false);

/// `arm_unverified_tls_check(event_emitter, parsed_globals)`.
pub(crate) fn arm_unverified_tls_check(globals: &crate::cli::GlobalArgs) {
    if globals.no_verify_ssl {
        UNVERIFIED_TLS_CHECK.store(true, Ordering::SeqCst);
    }
}

/// `refuse_bearer_signature` on `before-sign` of `service_id`.
pub(crate) fn before_sign(
    service_id: &str,
    version: &SignatureVersion,
) -> std::result::Result<(), SessionError> {
    if service_id == constants::SERVICE
        && UNVERIFIED_TLS_CHECK.load(Ordering::SeqCst)
        && matches!(version, SignatureVersion::Bearer)
    {
        return Err(SessionError::new(
            ErrorKind::General,
            format!(
                "TLS verification can't be disabled for a request signed with a bearer token, from {} or an IAM Identity Center sign-in: unlike a SigV4 signature, the bearer token can be replayed by anything that intercepts the connection. Drop --no-verify-ssl, or, if you use a TLS-inspecting proxy, trust its certificate authority with --ca-bundle instead. To sign with SigV4, do not select {} in AWS_AUTH_SCHEME_PREFERENCE or auth_scheme_preference.",
                constants::TOKEN_ENV_VAR,
                constants::BEARER_SCHEME
            ),
        ));
    }
    Ok(())
}

/// `reject_bearer_over_plain_http` on `before-send` of `service_id`.
pub(crate) fn before_send(
    service_id: &str,
    headers: &[(String, String)],
    url: &str,
) -> std::result::Result<(), SessionError> {
    if service_id != constants::SERVICE {
        return Ok(());
    }
    let bearer = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .is_some_and(|(_, v)| v.starts_with("Bearer "));
    if bearer {
        utils::require_https(url, "registry endpoint")
            .map_err(|m| SessionError::new(ErrorKind::General, m))?;
    }
    Ok(())
}

/// The `calling-command.agent-registry` handlers of a generated command:
/// `arm_unverified_tls_check_on_command`, and `redact_data_plane_call`
/// for the data plane operations.
pub(crate) fn calling_command(ctx: &Context, cli_service: &str, operation: &str) {
    if cli_service != constants::SERVICE {
        return;
    }
    arm_unverified_tls_check(&ctx.globals);
    if constants::DATA_PLANE_OPERATIONS.contains(&operation) {
        let token = std::env::var(constants::TOKEN_ENV_VAR).ok();
        utils::redact_logs_and_history(&token.iter().map(String::as_str).collect::<Vec<_>>());
    }
}

/// `hint_sign_out_on_unauthorized` on `after-call` of the data plane
/// operations.
pub(crate) fn after_call(model: &crate::model::ServiceModel, operation: &str, status: u16) {
    if model.name == constants::SERVICE && constants::DATA_PLANE_API_OPERATIONS.contains(&operation)
    {
        utils::hint_sign_out_on_unauthorized(status);
    }
}
