//! `ecs execute-command` (`awscli/customizations/ecs/executecommand.py`):
//! the `ExecuteCommand` operation, then `session-manager-plugin` with the
//! session.

use std::process::Command as Process;

use crate::cli::OperationHooks;
use crate::commands::services::exec::{self, After, Client};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::pyfmt::{json_dumps_default, str_repr};
use crate::value::{Map, Value};

const ERROR_MESSAGE: &str = "SessionManagerPlugin is not found. Please refer to SessionManager Documentation here: http://docs.aws.amazon.com/console/systems-manager/session-manager-plugin-not-found";
const TASK_NOT_FOUND: &str = "The task provided in the request was not found.";

struct Plain;

impl OperationHooks for Plain {}

fn python_error(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `subprocess.check_call(argv)`, with `ignore_user_entered_signals`
/// around the session.
fn check_call(ctx: &Context, argv: &[String], ignore_signals: bool) -> Result<(), CliError> {
    let mut saved = Vec::new();
    if ignore_signals {
        for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            saved.push((sig, unsafe { libc::signal(sig, libc::SIG_IGN) }));
        }
    }
    let status = Process::new(&argv[0]).args(&argv[1..]).status();
    for (sig, handler) in saved {
        unsafe {
            libc::signal(sig, handler);
        }
    }
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            let list: Vec<String> = argv.iter().map(|a| str_repr(a)).collect();
            let code = s.code().map_or_else(
                || {
                    use std::os::unix::process::ExitStatusExt as _;
                    format!("-{}", s.signal().unwrap_or_default())
                },
                |c| c.to_string(),
            );
            Err(python_error(
                ctx,
                &format!(
                    "Command '[{}]' returned non-zero exit status {code}.",
                    list.join(", ")
                ),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(python_error(ctx, ERROR_MESSAGE)),
        Err(e) => Err(python_error(ctx, &e.to_string())),
    }
}

fn get<'v>(ctx: &Context, v: &'v Value, key: &str) -> Result<&'v Value, CliError> {
    v.get(key)
        .ok_or_else(|| python_error(ctx, &format!("'{key}'")))
}

fn last_segment(v: &Value) -> String {
    crate::pyfmt::py_str(v)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `botocore.Session.profile`.
fn profile_name(ctx: &Context) -> String {
    ctx.globals
        .profile
        .clone()
        .or_else(|| std::env::var("AWS_DEFAULT_PROFILE").ok())
        .or_else(|| std::env::var("AWS_PROFILE").ok())
        .unwrap_or_default()
}

pub(super) fn execute_command(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let Some(inv) = exec::parse_operation(ctx, "ecs", "execute-command", args, &mut Plain)? else {
        return Ok(ExitCode::Success);
    };
    if inv.stub_response.is_some() {
        return exec::invoke(ctx, inv, &mut |_| Ok(After::Display));
    }
    ctx.session.set_command("ecs", "execute-command");
    check_call(ctx, &["session-manager-plugin".to_string()], false)?;
    let client = Client::new(ctx, "ecs")?;
    let response = client.call("ExecuteCommand", &inv.params)?;
    let region = ctx.session.region().unwrap_or_default();
    let endpoint_url = ctx
        .globals
        .endpoint_url
        .clone()
        .unwrap_or_else(|| format!("https://ecs.{region}.amazonaws.com"));
    let cluster = last_segment(get(ctx, &response, "clusterArn")?);
    let task = last_segment(get(ctx, &response, "taskArn")?);
    let container_name = get(ctx, &response, "containerName")?.clone();
    let mut p = Map::new();
    p.insert("cluster".to_string(), Value::str(&cluster));
    p.insert("tasks".to_string(), Value::List(vec![Value::str(&task)]));
    let tasks = client.call("DescribeTasks", &Value::Map(p))?;
    let list = get(ctx, &tasks, "tasks")?
        .as_list()
        .cloned()
        .unwrap_or_default();
    let Some(first) = list.first() else {
        return Err(python_error(ctx, TASK_NOT_FOUND));
    };
    let mut runtime_id = "None".to_string();
    for container in get(ctx, first, "containers")?
        .as_list()
        .into_iter()
        .flatten()
    {
        if get(ctx, container, "name")? == &container_name {
            runtime_id = crate::pyfmt::py_str(get(ctx, container, "runtimeId")?);
            break;
        }
    }
    let mut target = Map::new();
    target.insert(
        "Target".to_string(),
        Value::str(format!("ecs:{cluster}_{task}_{runtime_id}")),
    );
    let session = json_dumps_default(get(ctx, &response, "session")?);
    check_call(
        ctx,
        &[
            "session-manager-plugin".to_string(),
            session,
            region,
            "StartSession".to_string(),
            profile_name(ctx),
            json_dumps_default(&Value::Map(target)),
            endpoint_url,
        ],
        true,
    )?;
    Ok(ExitCode::Success)
}
