//! Runs one command line end to end (`awscli/clidriver.py`
//! `CLIDriver.main`, `ServiceOperation.__call__`, `CLIOperationCaller`;
//! `botocore/client.py` `_make_api_call`).

use std::io::{IsTerminal, Write};
use std::process::{Command as Process, Stdio};

use anyhow::Result;
use tokio::runtime::Runtime;

use crate::cli::{self, Command, GlobalArgs, OperationInvocation, WaitInvocation};
use crate::config::ConfigLookup;
use crate::endpoint::{self, BuiltinInputs, Builtins, EndpointError, S3Config};
use crate::error::{CliError, ExitCode};
use crate::handlers;
use crate::model::{Operation, ServiceModel};
use crate::output::errors::{
    ClientErrorInfo, ErrorOutputOptions, render_client_error_with, render_general_error,
    render_structured_error,
};
use crate::output::{self, Color, Format, OutputOptions, Response};
use crate::paginate;
use crate::protocol::{self, ParseOptions, TimestampFormat};
use crate::session::Session;
use crate::session::error::SessionError;
use crate::value::Value;
use crate::waiter;

/// Run the CLI with `args` (without the program name) and return the exit
/// code.
pub fn run(args: Vec<String>) -> i32 {
    let result = run_inner(args);
    crate::output::stdout::flush();
    match result {
        Ok(code) => code as i32,
        Err(e) => {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(e.message.as_bytes());
            let _ = stderr.flush();
            e.code as i32
        }
    }
}

pub(crate) struct Terminal {
    pub(crate) stdout_is_tty: bool,
    pub(crate) width: Option<usize>,
}

impl Terminal {
    fn detect() -> Terminal {
        let stdout_is_tty = std::io::stdout().is_terminal();
        Terminal {
            stdout_is_tty,
            width: terminal_width(),
        }
    }
}

fn terminal_width() -> Option<usize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
            Some(ws.ws_col as usize)
        } else {
            None
        }
    }
}

pub(crate) struct Context {
    pub(crate) globals: GlobalArgs,
    pub(crate) session: Session,
    pub(crate) runtime: Runtime,
    pub(crate) terminal: Terminal,
}

impl Context {
    pub(crate) fn error_format(&self) -> String {
        self.globals
            .cli_error_format
            .clone()
            .or_else(|| self.session.config_variable("cli_error_format"))
            .map(|f| f.to_lowercase())
            .unwrap_or_else(|| "enhanced".to_string())
    }

    pub(crate) fn color(&self) -> Color {
        match self.globals.color.as_deref() {
            Some("on") => Color::On,
            Some("off") => Color::Off,
            _ => Color::Auto,
        }
    }

    pub(crate) fn error_options(&self) -> ErrorOutputOptions {
        ErrorOutputOptions {
            color: self.color(),
            stdout_is_tty: self.terminal.stdout_is_tty,
            terminal_width: self.terminal.width,
        }
    }

    /// `FilteredExceptionHandler` for errors with structured info.
    pub(crate) fn structured_error(
        &self,
        code: ExitCode,
        error_code: &str,
        message: &str,
    ) -> CliError {
        CliError::new(
            code,
            render_structured_error(
                error_code,
                message,
                &self.error_format(),
                &self.error_options(),
            ),
        )
    }

    pub(crate) fn session_error(&self, e: &SessionError) -> CliError {
        match e.error_info() {
            Some((code, message)) => self.structured_error(e.exit_code(), &code, &message),
            None => CliError::new(e.exit_code(), render_general_error(&e.message)),
        }
    }

    pub(crate) fn general_error(&self, e: &anyhow::Error) -> CliError {
        if let Some(cli) = e.downcast_ref::<CliError>() {
            return CliError::new(cli.code, cli.message.clone());
        }
        if let Some(se) = e.downcast_ref::<SessionError>() {
            return self.session_error(se);
        }
        if let Some(pe) = e.downcast_ref::<handlers::ParamValidationError>() {
            return cli::render_fail(
                cli::Fail::ParamValidation(pe.to_string()),
                &self.globals,
                &self.session,
            );
        }
        if let Some(ee @ EndpointError::ParamValidation(_)) = e.downcast_ref::<EndpointError>() {
            return self.structured_error(ExitCode::Usage, "ParamValidation", &ee.to_string());
        }
        if let Some(ee) = e.downcast_ref::<EndpointError>() {
            let code = if ee.exit_code() == 252 {
                ExitCode::Usage
            } else {
                ExitCode::General
            };
            return CliError::new(code, render_general_error(&ee.to_string()));
        }
        CliError::new(ExitCode::General, render_general_error(&e.to_string()))
    }
}

fn run_inner(args: Vec<String>) -> Result<ExitCode, CliError> {
    let (globals, rest) = cli::parse_globals(&args)?;
    let session = Session::new(&globals).map_err(|e| match e.downcast::<SessionError>() {
        Ok(se) => se.to_cli_error(),
        Err(e) => CliError::new(ExitCode::General, render_general_error(&e.to_string())),
    })?;
    let command = cli::parse_command(&rest, &globals, &session)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| CliError::new(ExitCode::General, render_general_error(&e.to_string())))?;
    let ctx = Context {
        globals,
        session,
        runtime,
        terminal: Terminal::detect(),
    };
    match command {
        Command::Version => {
            println!("{}", version_string());
            Ok(ExitCode::Success)
        }
        Command::Print(text) => {
            write_stdout(&ctx, text.as_bytes())?;
            Ok(ExitCode::Success)
        }
        Command::Operation(inv) => run_operation(&ctx, inv),
        Command::Wait(inv) => run_wait(&ctx, inv),
        Command::Help(req) => crate::help::run(&ctx, &req),
        Command::Custom { path, args } => match crate::help::custom_request(&path, &args) {
            Some(req) => crate::help::run(&ctx, &req),
            None => crate::commands::run(&ctx, &path, &args),
        },
    }
}

fn version_string() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let system = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    };
    format!(
        "aws-cli/{} Python/3.14.8 {system}/{release} exe/{}",
        crate::model::REFERENCE_VERSION.trim(),
        std::env::consts::ARCH
    )
}

/// The outcome of one API call.
pub(crate) enum CallResult {
    Ok(Value),
    /// A service error: the parsed error response and the rendered info.
    ServiceError(Box<ClientErrorInfo>),
}

pub(crate) struct Caller<'a> {
    ctx: &'a Context,
    model: &'a ServiceModel,
    operation: Operation<'a>,
    builtins: Builtins,
    parse_options: ParseOptions,
}

impl<'a> Caller<'a> {
    pub(crate) fn new(
        ctx: &'a Context,
        model: &'a ServiceModel,
        operation_name: &str,
    ) -> Result<Caller<'a>> {
        let operation = model
            .operation(operation_name)
            .ok_or_else(|| anyhow::anyhow!("unknown operation {operation_name}"))?;
        let settings = ctx.session.client_settings()?;
        let config = ctx.session.config();
        let region = match settings.region.clone() {
            Some(r) => Some(r),
            None => {
                let endpoint_url = ctx
                    .globals
                    .endpoint_url
                    .clone()
                    .or_else(|| config.configured_endpoint_url(&model.name).ok().flatten());
                default_region(model, endpoint_url.is_some())?
            }
        };
        let account_id = None;
        let s3_get = |key: &str| config.get(&format!("s3.{key}")).ok().flatten();
        let s3_bool = |key: &str| config.get_bool(&format!("s3.{key}")).ok().flatten();
        let inputs = BuiltinInputs {
            region_name: region,
            endpoint_prefix: model.endpoint_prefix().to_string(),
            s3_config: S3Config {
                addressing_style: s3_get("addressing_style"),
                use_accelerate_endpoint: s3_bool("use_accelerate_endpoint"),
                use_dualstack_endpoint: s3_bool("use_dualstack_endpoint"),
                payload_signing_enabled: s3_bool("payload_signing_enabled"),
                use_arn_region: config
                    .get_bool("s3_use_arn_region")
                    .ok()
                    .flatten()
                    .or_else(|| s3_bool("use_arn_region")),
                s3_disable_multiregion_access_points: config
                    .get_bool("s3_disable_multiregion_access_points")
                    .ok()
                    .flatten()
                    .or_else(|| s3_bool("s3_disable_multiregion_access_points")),
            },
            use_fips_endpoint: config.get_bool("use_fips_endpoint").ok().flatten(),
            use_dualstack_endpoint: config.get_bool("use_dualstack_endpoint").ok().flatten(),
            endpoint_url: ctx
                .globals
                .endpoint_url
                .clone()
                .or_else(|| config.configured_endpoint_url(&model.name).ok().flatten()),
            account_id,
            account_id_endpoint_mode: config.get("account_id_endpoint_mode").ok().flatten(),
            s3_disable_express_session_auth: config
                .get_bool("s3_disable_express_session_auth")
                .ok()
                .flatten(),
            client_context_params: Default::default(),
        };
        let builtins = Builtins::compute(&inputs);
        let timestamp_format = match ctx
            .session
            .config_variable("cli_timestamp_format")
            .as_deref()
        {
            Some("wire") => TimestampFormat::Wire,
            _ => TimestampFormat::Iso8601,
        };
        Ok(Caller {
            ctx,
            model,
            operation,
            builtins,
            parse_options: ParseOptions { timestamp_format },
        })
    }

    /// `BaseClient._make_api_call` up to `_convert_to_request_dict`:
    /// `before-parameter-build`, `_resolve_endpoint_ruleset`, and the
    /// `ParamValidator` run over the parameters the handlers produced.
    fn prepare(
        &self,
        params: &Value,
    ) -> Result<(Value, handlers::CallContext, endpoint::Endpoint)> {
        let ctx = self.ctx;
        let mut params = params.clone();
        let call_ctx = handlers::before_parameter_build(
            self.model,
            &self.operation,
            &mut params,
            &ctx.session,
        )?;

        let mut builtins = self.builtins.clone();
        if self.model.name == "s3" {
            endpoint::customize_s3_builtins(&mut builtins, self.operation.name, &params);
        }
        let use_ssl = !ctx
            .globals
            .endpoint_url
            .as_deref()
            .is_some_and(|u| u.starts_with("http://"));
        let endpoint =
            endpoint::construct_endpoint(self.model, &self.operation, &params, &builtins, use_ssl)?;
        if let Some(input) = self.operation.input()
            && let Some(report) = cli::validate::report(&params, &input)
        {
            return Err(handlers::ParamValidationError(report).into());
        }
        Ok((params, call_ctx, endpoint))
    }

    /// [`Caller::call`] up to the `before-call` handler that answers
    /// `--generate-cli-skeleton output` with a stub response.
    pub(crate) fn check(&self, params: &Value) -> Result<()> {
        self.prepare(params).map(|_| ())
    }

    /// `BaseClient._make_api_call`.
    pub(crate) fn call(&self, params: &Value) -> Result<CallResult> {
        let ctx = self.ctx;
        let (params, call_ctx, mut endpoint) = self.prepare(params)?;

        let mut request = protocol::serialize(self.model, &self.operation, &params)?;
        for (name, values) in &endpoint.headers {
            if let Some(first) = values.first() {
                handlers::set_header(&mut request.headers, name, first);
            }
        }
        handlers::set_header(&mut request.headers, "User-Agent", "");
        let checksum =
            handlers::resolve_request_checksum(&self.operation, &params, &request, &ctx.session)?;
        let url = ctx.session.request_url(&endpoint, &request)?;
        let region = ctx.session.client_settings()?.region;
        let signing_name = endpoint
            .properties
            .get("authSchemes")
            .and_then(|s| s.as_array())
            .and_then(|s| s.first())
            .and_then(|s| s.get("signingName"))
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| self.model.signing_name())
            .to_string();
        let credentials = || -> Result<Option<crate::session::sign::Credentials>> {
            Ok(block_on(
                &ctx.runtime,
                ctx.session.credentials(region.as_deref()),
            )?)
        };
        let presign = handlers::PresignEnv {
            url: &url,
            region: region.as_deref(),
            signing_name: &signing_name,
            credentials: &credentials,
            unsigned: ctx.globals.no_sign_request,
            now: chrono::Utc::now(),
        };
        handlers::before_call(self.model, &self.operation, &params, &mut request, &presign)?;
        handlers::maybe_compress_request(&self.operation, &mut request, &ctx.session)?;
        if let Some(checksum) = &checksum {
            handlers::apply_request_checksum(&mut request, checksum)?;
        }
        endpoint.headers.clear();
        let url = ctx.session.request_url(&endpoint, &request)?;
        if let Some(new_url) =
            handlers::request_created(self.model, &self.operation, &mut request, &url)
        {
            endpoint.url = new_url;
            request.url_path = String::new();
            request.query.clear();
            request.host_prefix = None;
        }

        let adjust = |response: &mut protocol::HttpResponse| {
            handlers::adjust_attempt_response(self.model, &self.operation, response)
        };
        let outcome = block_on(
            &ctx.runtime,
            ctx.session
                .send_adjusted(self.model, &self.operation, &endpoint, &request, &adjust),
        )?;
        let mut response = outcome.response;
        let customized = handlers::before_parse(self.model, &self.operation, &mut response);
        let failed = response.status >= 300;
        let (mut parsed, error_code) = if failed {
            let error =
                protocol::parse_error(self.model, &self.operation, &response, &self.parse_options)?;
            (error.parsed, error.code)
        } else {
            (
                protocol::parse(self.model, &self.operation, &response, &self.parse_options)?,
                String::new(),
            )
        };
        if let Some(map) = parsed.as_map_mut() {
            map.extend(customized);
        }
        set_retry_attempts(&mut parsed, outcome.attempts);
        handlers::after_call(
            self.model,
            &self.operation,
            &mut parsed,
            &response,
            &call_ctx,
            !ctx.globals.no_paginate,
        )?;
        if !failed {
            return Ok(CallResult::Ok(parsed));
        }
        let modeled_fields = modeled_error_fields(self.model, &error_code);
        let code = parsed
            .get("Error")
            .and_then(|e| e.get("Code"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or("Unknown")
            .to_string();
        let message = parsed
            .get("Error")
            .and_then(|e| e.get("Message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "Unknown".to_string());
        Ok(CallResult::ServiceError(Box::new(ClientErrorInfo {
            operation_name: self.operation.name.to_string(),
            code,
            message,
            max_retries_reached: outcome
                .max_attempts_reached
                .then(|| outcome.attempts.saturating_sub(1)),
            parsed,
            modeled_fields: Some(modeled_fields),
        })))
    }
}

fn set_retry_attempts(parsed: &mut Value, attempts: u32) {
    if let Some(meta) = parsed
        .as_map_mut()
        .and_then(|m| m.get_mut("ResponseMetadata"))
        .and_then(Value::as_map_mut)
        && meta.contains_key("RetryAttempts")
    {
        meta.insert(
            "RetryAttempts".to_string(),
            Value::int(i64::from(attempts.saturating_sub(1))),
        );
    }
}

/// `{'Code', 'Message'} | set(error_shape.members)`.
fn modeled_error_fields(model: &ServiceModel, code: &str) -> Vec<String> {
    let mut fields = vec!["Code".to_string(), "Message".to_string()];
    let shape = model.shape_names().into_iter().find_map(|name| {
        let shape = model.shape(name)?;
        if !shape.get_bool("exception") {
            return None;
        }
        let error_code = shape
            .shape_get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or(name);
        (error_code == code).then_some(shape)
    });
    if let Some(shape) = shape {
        for (name, _) in shape.members() {
            if !fields.iter().any(|f| f == name) {
                fields.push(name.to_string());
            }
        }
    }
    fields
}

pub(crate) fn client_error(ctx: &Context, info: &ClientErrorInfo) -> CliError {
    CliError::new(
        ExitCode::Service,
        render_client_error_with(info, &ctx.error_format(), &ctx.error_options()),
    )
}

fn run_operation(ctx: &Context, inv: OperationInvocation) -> Result<ExitCode, CliError> {
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
        let caller = Caller::new(ctx, &model, &inv.operation).map_err(|e| ctx.general_error(&e))?;
        caller
            .check(&inv.params)
            .map_err(|e| ctx.general_error(&e))?;
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
                let mut service_error: Option<Box<ClientErrorInfo>> = None;
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
                        if let Some(info) = service_error {
                            return Err(client_error(ctx, &info));
                        }
                        return Err(ctx.general_error(&e));
                    }
                }
            }
            None => match caller
                .call(&inv.params)
                .map_err(|e| ctx.general_error(&e))?
            {
                CallResult::Ok(mut v) => {
                    if let Some(outfile) = &inv.outfile {
                        save_streaming_output(&operation, &mut v, outfile)
                            .map_err(|e| ctx.general_error(&e))?;
                    }
                    Response::Single(v)
                }
                CallResult::ServiceError(info) => return Err(client_error(ctx, &info)),
            },
        }
    };
    display_response(ctx, &inv.operation, response)?;
    Ok(ExitCode::Success)
}

/// `StreamingOutputArgument.save_file`.
fn save_streaming_output(
    operation: &Operation<'_>,
    parsed: &mut Value,
    outfile: &str,
) -> Result<()> {
    let Some(output) = operation.output() else {
        return Ok(());
    };
    let Some(key) = output.get_str("payload") else {
        return Ok(());
    };
    let Some(map) = parsed.as_map_mut() else {
        return Ok(());
    };
    let Some(body) = map.shift_remove(key) else {
        return Ok(());
    };
    let bytes = match body {
        Value::Bytes(b) => b,
        Value::Str(s) => s.into_bytes(),
        _ => Vec::new(),
    };
    std::fs::write(outfile, bytes)?;
    Ok(())
}

/// `CLIOperationCaller._display_response`.
pub(crate) fn display_response(
    ctx: &Context,
    command_name: &str,
    response: Response,
) -> Result<(), CliError> {
    let output = ctx
        .globals
        .output
        .clone()
        .or_else(|| ctx.session.config_variable("output"))
        .unwrap_or_else(|| "json".to_string());
    let Some(format) = Format::parse(&output) else {
        return Err(CliError::new(
            ExitCode::General,
            render_general_error(&format!("Unknown output type: {output}")),
        ));
    };
    let options = OutputOptions {
        format,
        query: ctx.globals.query.clone(),
        color: ctx.color(),
        terminal_width: ctx.terminal.width,
        stdout_is_tty: ctx.terminal.stdout_is_tty,
    };
    let mut buf = Vec::new();
    let result = output::write_response(command_name, response, &options, &mut buf);
    write_stdout(ctx, &buf)?;
    if let Err(e) = result {
        return Err(ctx.general_error(&e));
    }
    Ok(())
}

/// `OutputStreamFactory.get_output_stream`: the pager when stdout is a tty
/// and a pager is configured, else stdout.
pub(crate) fn write_stdout(ctx: &Context, bytes: &[u8]) -> Result<(), CliError> {
    let pager = ctx
        .session
        .config_variable("pager")
        .filter(|p| !p.is_empty());
    if ctx.terminal.stdout_is_tty
        && let Some(pager) = pager
        && !bytes.is_empty()
    {
        let mut child = Process::new("sh");
        child.arg("-c").arg(&pager).stdin(Stdio::piped());
        if std::env::var_os("LESS").is_none() {
            child.env("LESS", "FRX");
        }
        match child.spawn() {
            Ok(mut proc) => {
                if let Some(mut stdin) = proc.stdin.take() {
                    let _ = stdin.write_all(bytes);
                }
                let _ = proc.wait();
                return Ok(());
            }
            Err(e) => {
                let message = format!(
                    "Unable to redirect output to pager. Received the following error when opening pager:\n{e}\n\nLearn more about configuring the output pager by running \"aws help config-vars\"."
                );
                return Err(ctx.structured_error(ExitCode::Config, "Pager", &message));
            }
        }
    }
    crate::output::stdout::write_flushed(bytes);
    Ok(())
}

fn run_wait(ctx: &Context, inv: WaitInvocation) -> Result<ExitCode, CliError> {
    let mut model = ServiceModel::load(&inv.service).map_err(|e| ctx.general_error(&e))?;
    model.apply_endpoint_ruleset_customizations();
    let config = model
        .waiters()
        .and_then(|w| w.get(&inv.waiter))
        .and_then(|w| w.as_object())
        .ok_or_else(|| ctx.general_error(&anyhow::anyhow!("unknown waiter {}", inv.waiter)))?;
    let operation_name = config
        .get("operation")
        .and_then(|o| o.as_str())
        .unwrap_or_default()
        .to_string();
    ctx.session.set_command(
        &inv.cli_service,
        &format!("wait.{}", cli::arguments::xform_name(&inv.waiter, '-')),
    );
    let caller = Caller::new(ctx, &model, &operation_name).map_err(|e| ctx.general_error(&e))?;
    let mut poll = |params: &Value| -> Result<Value> {
        let result = caller.call(params)?;
        Ok(match result {
            CallResult::Ok(v) => v,
            CallResult::ServiceError(info) => info.parsed,
        })
    };
    let outcome = ctx
        .runtime
        .block_on(waiter::wait(&inv.waiter, config, &inv.params, &mut poll))
        .map_err(|e| ctx.general_error(&e))?;
    if let Some(stderr) = outcome.stderr() {
        return Err(CliError::new(
            exit_code_from_i32(outcome.exit_code()),
            stderr,
        ));
    }
    Ok(ExitCode::Success)
}

fn exit_code_from_i32(code: i32) -> ExitCode {
    match code {
        0 => ExitCode::Success,
        1 => ExitCode::S3TransferFailed,
        2 => ExitCode::S3Skipped,
        130 => ExitCode::Interrupted,
        252 => ExitCode::Usage,
        253 => ExitCode::Config,
        254 => ExitCode::Service,
        _ => ExitCode::General,
    }
}

/// Wait for `future` from synchronous code, inside or outside the runtime.
pub(crate) fn block_on<F: std::future::Future>(runtime: &Runtime, future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => runtime.block_on(future),
    }
}

/// The region `ClientEndpointBridge.resolve` gives a client created without
/// one: the service's `partitionEndpoint` in the `aws` partition of
/// `endpoints.json` (kept only when no endpoint URL is given,
/// `_pick_region_values`), else `NoRegionError`.
fn default_region(model: &ServiceModel, has_endpoint_url: bool) -> Result<Option<String>> {
    let partition_endpoint = crate::model::endpoints()["partitions"]
        .as_array()
        .and_then(|p| p.iter().find(|p| p["partition"] == "aws"))
        .and_then(|p| p["services"].get(model.endpoint_prefix()))
        .and_then(|s| s.get("partitionEndpoint"))
        .and_then(|e| e.as_str());
    match partition_endpoint {
        Some(endpoint) if !has_endpoint_url => Ok(Some(endpoint.to_string())),
        Some(_) => Ok(None),
        None => Err(SessionError::new(
            crate::session::ErrorKind::NoRegion,
            "You must specify a region.",
        )
        .into()),
    }
}
