//! `ssm start-session` (`awscli/customizations/sessionmanager.py`): the
//! `StartSession` operation whose caller hands the session to
//! `session-manager-plugin`.

use std::process::{Command, Stdio};

use super::exec::{self, Client};
use super::presign;
use crate::cli::OperationHooks;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::pyfmt::{json_dumps_default, str_repr};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "start-session" => start_session(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

struct Plain;

impl OperationHooks for Plain {}

const PLUGIN: &str = "session-manager-plugin";
const LAST_PLUGIN_VERSION_WITHOUT_ENV_VAR: &str = "1.2.497.0";
const RECOMMENDED_MINIMUM_PLUGIN_VERSION: &str = "1.2.764.0";
const DEFAULT_SSM_ENV_NAME: &str = "AWS_SSM_START_SESSION_RESPONSE";
const OUTDATED_PLUGIN_VERSION_MESSAGE: &str = "\nWARNING: An outdated SessionManagerPlugin version detected. Please upgrade it to the latest version. \nFor more information, refer:\n  https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html\n\n";

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `VersionRequirement`: `None` when the version does not parse.
fn version_parts(version: &str) -> Option<Vec<u64>> {
    let v: String = version.chars().filter(|c| !c.is_whitespace()).collect();
    let parts: Vec<&str> = v.split('.').collect();
    if parts.is_empty()
        || parts.len() > 4
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    parts.iter().map(|p| p.parse().ok()).collect()
}

/// `VersionRequirement(min).meets_requirement(version, inclusive)`.
fn meets_requirement(version: &str, min: &str, inclusive: bool) -> bool {
    let Some(mut a) = version_parts(version) else {
        return false;
    };
    let mut b = version_parts(min).expect("valid minimum");
    while a.len() != b.len() {
        if a.len() > b.len() {
            b.push(0);
        } else {
            a.push(0);
        }
    }
    if inclusive { a >= b } else { a > b }
}

fn list_repr(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| str_repr(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn os_error(ctx: &Context, e: &std::io::Error) -> CliError {
    let text = e.to_string();
    let text = text
        .find(" (os error")
        .map_or(text.as_str(), |i| &text[..i])
        .to_string();
    general(
        ctx,
        &format!(
            "[Errno {}] {text}: {}",
            e.raw_os_error().unwrap_or(0),
            str_repr(PLUGIN)
        ),
    )
}

/// `ignore_user_entered_signals`: SIGINT, SIGQUIT, and SIGTSTP are ignored
/// while the plugin runs.
fn with_signals_ignored<T>(f: impl FnOnce() -> T) -> T {
    let signals = [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP];
    let previous: Vec<libc::sighandler_t> = signals
        .iter()
        .map(|&s| unsafe { libc::signal(s, libc::SIG_IGN) })
        .collect();
    let r = f();
    for (s, p) in signals.iter().zip(previous) {
        unsafe {
            libc::signal(*s, p);
        }
    }
    r
}

/// `StartSessionCaller.invoke`.
fn start_session(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let Some(inv) = exec::parse_operation(ctx, "ssm", "start-session", args, &mut Plain)? else {
        return Ok(ExitCode::Success);
    };
    ctx.session.set_command("ssm", "start-session");
    let client = Client::new(ctx, "ssm")?;
    let response = client.call("StartSession", &inv.params)?;
    let field = |k: &str| {
        response
            .get(k)
            .cloned()
            .ok_or_else(|| general(ctx, &str_repr(k)))
    };
    let session_id = field("SessionId")?;
    let region = presign::region(ctx)?.unwrap_or_default();
    let profile = ctx.globals.profile.clone().unwrap_or_default();
    let endpoint_url =
        presign::resolve_endpoint(ctx, "ssm", "StartSession", ctx.globals.endpoint_url.clone())?;
    let mut session = Map::new();
    session.insert("SessionId".into(), session_id.clone());
    session.insert("TokenValue".into(), field("TokenValue")?);
    session.insert("StreamUrl".into(), field("StreamUrl")?);
    let mut start_response = json_dumps_default(&Value::Map(session));

    let version = match Command::new(PLUGIN)
        .arg("--version")
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
    {
        Ok(out) => {
            if !out.status.success() {
                return Err(general(
                    ctx,
                    &format!(
                        "Command '{}' returned non-zero exit status {}.",
                        list_repr(&[PLUGIN.to_string(), "--version".to_string()]),
                        out.status.code().unwrap_or(-1)
                    ),
                ));
            }
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut params = Map::new();
            params.insert("SessionId".into(), session_id);
            client.call("TerminateSession", &Value::Map(params))?;
            return Err(general(
                ctx,
                "SessionManagerPlugin is not found. Please refer to SessionManager Documentation here: http://docs.aws.amazon.com/console/systems-manager/session-manager-plugin-not-found",
            ));
        }
        Err(e) => return Err(os_error(ctx, &e)),
    };
    if version_parts(&version).is_some()
        && !meets_requirement(&version, RECOMMENDED_MINIMUM_PLUGIN_VERSION, true)
    {
        super::basic::eprint(OUTDATED_PLUGIN_VERSION_MESSAGE);
    }
    let mut command = Command::new(PLUGIN);
    if meets_requirement(&version, LAST_PLUGIN_VERSION_WITHOUT_ENV_VAR, false) {
        command.env(DEFAULT_SSM_ENV_NAME, &start_response);
        start_response = DEFAULT_SSM_ENV_NAME.to_string();
    }
    let argv = vec![
        PLUGIN.to_string(),
        start_response,
        region,
        "StartSession".to_string(),
        profile,
        json_dumps_default(&inv.params),
        endpoint_url,
    ];
    command.args(&argv[1..]);
    let status = with_signals_ignored(|| command.status());
    match status {
        Ok(s) if s.success() => Ok(ExitCode::Success),
        Ok(s) => Err(general(
            ctx,
            &format!(
                "Command '{}' returned non-zero exit status {}.",
                list_repr(&argv),
                s.code().unwrap_or(-1)
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(general(
            ctx,
            "SessionManagerPlugin is not found. Please refer to SessionManager Documentation here: http://docs.aws.amazon.com/console/systems-manager/session-manager-plugin-not-found",
        )),
        Err(e) => Err(os_error(ctx, &e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(meets_requirement("1.2.764.0\n", "1.2.764.0", true));
        assert!(!meets_requirement("1.2.764.0", "1.2.764.0", false));
        assert!(meets_requirement("1.3", "1.2.497.0", false));
        assert!(!meets_requirement("1.2.x", "1.2.497.0", false));
        assert!(version_parts("1.2.3.4.5").is_none());
    }
}
