//! Calling the service from a customization: the client a command
//! creates (`session.create_client(...)` + the client method, with
//! botocore's validation), and running a customized operation's
//! invocation (`CLIOperationCaller.invoke` + `_display_response`) with the
//! customization's `after-call` handler.

use anyhow::Result;

use crate::cli::{self, Command, OperationHooks, OperationInvocation};
use crate::driver::{
    CallResult, Caller, ClientConfig, Context, client_error, display_response, write_stdout,
};
use crate::error::{CliError, ExitCode};
use crate::model::ServiceModel;
use crate::output::Response;
use crate::paginate::{self, PaginationConfig};
use crate::value::Value;

/// A botocore client for one service.
pub(crate) struct Client<'c> {
    pub ctx: &'c Context,
    pub model: ServiceModel,
    pub config: ClientConfig,
}

impl<'c> Client<'c> {
    pub(crate) fn new(ctx: &'c Context, service: &str) -> Result<Client<'c>, CliError> {
        Client::with_config(ctx, service, ClientConfig::default())
    }

    /// `session.create_client(service, region_name=..., endpoint_url=...)`.
    pub(crate) fn with_config(
        ctx: &'c Context,
        service: &str,
        config: ClientConfig,
    ) -> Result<Client<'c>, CliError> {
        let mut model = ServiceModel::load(service).map_err(|e| ctx.general_error(&e))?;
        model.apply_endpoint_ruleset_customizations();
        Ok(Client { ctx, model, config })
    }

    /// `client.<operation>(**params)`: the parsed response, or the
    /// rendered `ClientError`.
    pub(crate) fn call(&self, operation: &str, params: &Value) -> Result<Value, CliError> {
        match self.try_call(operation, params)? {
            CallResult::Ok(v) => Ok(v),
            CallResult::ServiceError(info) => Err(client_error(self.ctx, &info)),
        }
    }

    /// [`Client::call`] keeping a service error as a value (for commands
    /// that catch `ClientError`).
    pub(crate) fn try_call(&self, operation: &str, params: &Value) -> Result<CallResult, CliError> {
        self.call_raw(operation, params)
            .map_err(|e| self.ctx.general_error(&e))
    }

    /// [`Client::try_call`] keeping the exception as raised.
    pub(crate) fn call_raw(&self, operation: &str, params: &Value) -> Result<CallResult> {
        Caller::with_config(self.ctx, &self.model, operation, self.config.clone())?.call(params)
    }

    /// `client.get_paginator(operation).paginate(**params)`: every page.
    pub(crate) fn paginate(&self, operation: &str, params: &Value) -> Result<Vec<Value>, CliError> {
        let Some(paginator) = self.model.paginator(operation) else {
            return Ok(vec![self.call(operation, params)?]);
        };
        let op = self.model.operation(operation).expect("operation");
        let caller = Caller::new(self.ctx, &self.model, operation)
            .map_err(|e| self.ctx.general_error(&e))?;
        let mut service_error = None;
        let mut call = |p: &Value| -> Result<Value> {
            match caller.call(p)? {
                CallResult::Ok(v) => Ok(v),
                CallResult::ServiceError(info) => {
                    service_error = Some(info);
                    Err(anyhow::anyhow!("service error"))
                }
            }
        };
        let result = paginate::paginate(
            paginator,
            params,
            &PaginationConfig::default(),
            paginate::limit_key_type(&op, paginator),
            &mut call,
        );
        match result {
            Ok(pages) => Ok(pages.pages),
            Err(e) => match service_error {
                Some(info) => Err(client_error(self.ctx, &info)),
                None => Err(self.ctx.general_error(&e)),
            },
        }
    }
}

/// Parse the customized operation `cli_service name` with `hooks`:
/// `None` when the command printed its output (help,
/// `--generate-cli-skeleton`).
pub(crate) fn parse_operation(
    ctx: &Context,
    cli_service: &str,
    name: &str,
    args: &[String],
    hooks: &mut dyn OperationHooks,
) -> Result<Option<OperationInvocation>, CliError> {
    match cli::parse_operation_with(cli_service, name, args, &ctx.globals, &ctx.session, hooks)? {
        Command::Print(text) => {
            write_stdout(ctx, text.as_bytes())?;
            Ok(None)
        }
        Command::Operation(inv) => Ok(Some(inv)),
        _ => unreachable!("operation command"),
    }
}

/// What an `after-call` handler leaves to display.
pub(crate) enum After {
    /// Display the (possibly changed) response.
    Display,
    /// Nothing more to write.
    Done,
}

/// `ServiceOperation.__call__` from the invocation on: call the service
/// (paginating when enabled), run `after` on a single response, and
/// display the response.
pub(crate) fn invoke(
    ctx: &Context,
    inv: OperationInvocation,
    after: &mut dyn FnMut(&mut Value) -> Result<After, CliError>,
) -> Result<ExitCode, CliError> {
    let mut model = ServiceModel::load(&inv.service).map_err(|e| ctx.general_error(&e))?;
    model.apply_endpoint_ruleset_customizations();
    let operation = model
        .operation(&inv.operation)
        .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("unknown operation")))?;
    ctx.session.set_command(
        &inv.cli_service,
        &cli::arguments::xform_name(&inv.operation, '-'),
    );
    let response = if let Some(stub) = inv.stub_response.clone() {
        Response::Single(stub)
    } else {
        let caller = Caller::new(ctx, &model, &inv.operation).map_err(|e| ctx.general_error(&e))?;
        let paginator = if inv.paginate {
            model.paginator(&inv.operation)
        } else {
            None
        };
        match paginator {
            Some(paginator) => {
                let mut service_error = None;
                let mut call = |params: &Value| -> Result<Value> {
                    match caller.call(params)? {
                        CallResult::Ok(v) => Ok(v),
                        CallResult::ServiceError(info) => {
                            service_error = Some(info);
                            Err(anyhow::anyhow!("service error"))
                        }
                    }
                };
                let result = paginate::paginate(
                    paginator,
                    &inv.params,
                    &inv.pagination,
                    paginate::limit_key_type(&operation, paginator),
                    &mut call,
                );
                match result {
                    Ok(pages) => Response::Paginated(pages),
                    Err(e) => {
                        return Err(match service_error {
                            Some(info) => client_error(ctx, &info),
                            None => ctx.general_error(&e),
                        });
                    }
                }
            }
            None => match caller
                .call(&inv.params)
                .map_err(|e| ctx.general_error(&e))?
            {
                CallResult::Ok(mut v) => match after(&mut v)? {
                    After::Display => Response::Single(v),
                    After::Done => return Ok(ExitCode::Success),
                },
                CallResult::ServiceError(info) => return Err(client_error(ctx, &info)),
            },
        }
    };
    display_response(ctx, &inv.operation, response)?;
    Ok(ExitCode::Success)
}

/// Parse and run a customized operation whose only changes are in its
/// arguments.
pub(crate) fn run_operation(
    ctx: &Context,
    cli_service: &str,
    name: &str,
    args: &[String],
    hooks: &mut dyn OperationHooks,
) -> Result<ExitCode, CliError> {
    match parse_operation(ctx, cli_service, name, args, hooks)? {
        Some(inv) => invoke(ctx, inv, &mut |_| Ok(After::Display)),
        None => Ok(ExitCode::Success),
    }
}

/// The context of a client a customization creates with arguments other
/// than the global ones (`session.create_client(service, region_name=...,
/// endpoint_url=..., aws_access_key_id=...)`): `endpoint_url` replaces
/// `--endpoint-url` (`None` resolves the endpoint from the configuration),
/// `region` replaces `--region` when given, and `credentials`
/// (access key, secret key, session token) replace the credential chain.
pub(crate) fn sub_context(
    ctx: &Context,
    endpoint_url: Option<String>,
    region: Option<String>,
    credentials: Option<(String, String, Option<String>)>,
) -> Result<Context, CliError> {
    let mut globals = ctx.globals.clone();
    globals.endpoint_url = endpoint_url;
    if region.is_some() {
        globals.region = region;
    }
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    if let Some((access_key, secret_key, token)) = credentials {
        globals.profile = None;
        env.insert("AWS_ACCESS_KEY_ID".to_string(), access_key);
        env.insert("AWS_SECRET_ACCESS_KEY".to_string(), secret_key);
        match token {
            Some(t) => env.insert("AWS_SESSION_TOKEN".to_string(), t),
            None => env.remove("AWS_SESSION_TOKEN"),
        };
    }
    let session =
        crate::session::Session::with_env(&globals, env).map_err(|e| ctx.session_error(&e))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| ctx.general_error(&e.into()))?;
    Ok(Context {
        globals,
        session,
        runtime,
        terminal: crate::driver::Terminal {
            stdout_is_tty: ctx.terminal.stdout_is_tty,
            width: ctx.terminal.width,
        },
    })
}
