//! `ecs` customizations (`awscli/customizations/ecs/`): the
//! `--monitor-resources` / `--monitor-mode` arguments of the express
//! gateway service mutations (`monitormutatinggatewayservice.py`).

mod deploy;
mod execute;

use std::io::IsTerminal;

use indexmap::IndexMap;

use super::exec::{self, After};
use crate::cli::argparse::{Nargs, NsVal};
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::value::Value;

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    let default_view = match name {
        "create-express-gateway-service" | "update-express-gateway-service" => "DEPLOYMENT",
        "delete-express-gateway-service" => "RESOURCE",
        "execute-command" => return execute::execute_command(ctx, args),
        "deploy" => return deploy::deploy(ctx, args),
        _ => return Err(crate::commands::not_implemented()),
    };
    let mut hooks = Monitor {
        default_view,
        view: None,
        mode: String::new(),
    };
    let Some(inv) = exec::parse_operation(ctx, "ecs", name, args, &mut hooks)? else {
        return Ok(ExitCode::Success);
    };
    exec::invoke(ctx, inv, &mut |parsed| hooks.after_call(ctx, parsed))
}

struct Monitor {
    default_view: &'static str,
    /// `effective_resource_view`.
    view: Option<String>,
    /// `effective_mode`.
    mode: String,
}

impl Monitor {
    /// `MonitorMutatingGatewayService.after_call` (for a successful
    /// response).
    fn after_call(&mut self, ctx: &Context, parsed: &mut Value) -> Result<After, CliError> {
        if self.view.is_none() {
            return Ok(After::Display);
        }
        let arn = parsed
            .get("service")
            .and_then(|s| s.get("serviceArn"))
            .filter(|v| v.is_truthy());
        if arn.is_none() {
            return Ok(After::Display);
        }
        let tty = std::io::stdout().is_terminal();
        if self.mode == "INTERACTIVE" && !tty {
            super::basic::eprint(
                "awsr: [ERROR]: Interactive mode requires a TTY (terminal). Monitoring skipped. Use --monitor-mode TEXT-ONLY for non-interactive environments.",
            );
            return Ok(After::Display);
        }
        let _ = ctx;
        Err(crate::commands::not_implemented())
    }
}

impl OperationHooks for Monitor {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert(
            "monitor-resources".to_string(),
            Arg::custom_argument(
                "monitor-resources",
                CustomSpec {
                    nargs: Some(Nargs::Optional),
                    choices: vec!["DEPLOYMENT".to_string(), "RESOURCE".to_string()],
                    dest: Some("monitor_resources".to_string()),
                    const_: Some("__DEFAULT__".to_string()),
                    ..CustomSpec::default()
                },
                false,
            ),
        );
        table.insert(
            "monitor-mode".to_string(),
            Arg::custom_argument(
                "monitor-mode",
                CustomSpec {
                    nargs: Some(Nargs::Optional),
                    choices: vec!["INTERACTIVE".to_string(), "TEXT-ONLY".to_string()],
                    dest: Some("monitor_mode".to_string()),
                    ..CustomSpec::default()
                },
                false,
            ),
        );
    }

    /// `_parse_and_validate_monitoring_args`.
    fn args_parsed(&mut self, ns: &IndexMap<String, NsVal>, _: &mut bool) -> Result<(), Fail> {
        self.view = match ns.get("monitor_resources") {
            Some(NsVal::Str(s)) if s == "__DEFAULT__" => Some(self.default_view.to_string()),
            Some(NsVal::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let mode = match ns.get("monitor_mode") {
            Some(NsVal::Str(s)) => Some(s.clone()),
            _ => None,
        };
        if mode.is_some() && self.view.is_none() {
            return Err(Fail::Python(
                "--monitor-mode can only be used with --monitor-resources".to_string(),
            ));
        }
        self.mode = mode
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "INTERACTIVE".to_string());
        Ok(())
    }
}
