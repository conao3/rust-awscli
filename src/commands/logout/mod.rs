//! `aws logout` (`awscli/customizations/login/logout.py`): remove cached
//! console login credentials.

use crate::commands::configure::basic::{self, ArgSpec, ArgsExt, Outcome};
use crate::commands::configure::{err, fail, out};
use crate::config::ini::IniValue;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::session::{cache, login_token};
use crate::value::Value;

pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let args =
        match basic::parse_args(args, &[ArgSpec::flag("all")], &[]).map_err(|f| fail(ctx, f))? {
            Outcome::Help => {
                out("logout\n");
                return Ok(ExitCode::Success);
            }
            Outcome::Run(a) => a,
        };
    let config = ctx.session.config();
    let cache = cache::login_cache(config);
    if args.flag("all") {
        let mut deleted = 0;
        for (path, contents) in crate::commands::sso::json_files(cache.dir()) {
            let is_login = contents
                .get("accessToken")
                .is_some_and(|a| matches!(a, Value::Map(m) if m.contains_key("accessKeyId")));
            if is_login && std::fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
        }
        if deleted > 0 {
            let plural = if deleted != 1 { "s" } else { "" };
            out(&format!(
                "Removed {deleted} cached login credential{plural}. Note, any local developer tools that have already loaded access tokens may continue to use them until their expiration. Access tokens expire in 15 minutes.\n"
            ));
        } else {
            out("No cached login session tokens found.\n");
        }
        return Ok(ExitCode::Success);
    }
    let profile = config.profile().unwrap_or_else(|| "default".to_string());
    let session_id = config
        .full_config()
        .profiles
        .get(&profile)
        .and_then(|p| p.get("login_session"))
        .and_then(IniValue::as_str)
        .map(str::to_string);
    let Some(session_id) = session_id else {
        err(&format!(
            "warning: no login session found for profile '{profile}'\n"
        ));
        return Ok(ExitCode::Success);
    };
    let path = cache
        .dir()
        .join(format!("{}.json", login_token::cache_key(&session_id)));
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    out(&format!(
        "Removed cached login credentials for profile '{profile}'. Note, any local developer tools that have already loaded the access token may continue to use it until its expiration. Access tokens expire in 15 minutes.\n"
    ));
    Ok(ExitCode::Success)
}
