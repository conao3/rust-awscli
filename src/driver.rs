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
    if crate::output::colorama::initialized() && std::io::stdout().is_terminal() {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(b"\x1b[0m");
        let _ = stdout.flush();
    }
    match result {
        Ok(code) => code,
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
    pub(crate) runtime: LazyRuntime,
    pub(crate) terminal: Terminal,
}

#[derive(Default)]
pub(crate) struct LazyRuntime(std::sync::OnceLock<Runtime>);

impl std::ops::Deref for LazyRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        self.0.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("tokio runtime")
        })
    }
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

/// Python's recursion limit as it shows for an alias that expands to
/// itself.
const MAX_ALIAS_DEPTH: usize = 200;

fn new_session(globals: &GlobalArgs) -> Result<Session, CliError> {
    Session::new(globals).map_err(|e| match e.downcast::<SessionError>() {
        Ok(se) => se.to_cli_error(),
        Err(e) => CliError::new(ExitCode::General, render_general_error(&e.to_string())),
    })
}

pub fn help_request(args: &[String]) -> Option<crate::help::HelpRequest> {
    let (globals, rest) = cli::parse_globals(args).ok()?;
    let session = new_session(&globals).ok()?;
    match cli::parse_command_shadowed(&rest, &globals, &session, &[], None).ok()? {
        Command::Help(req) => Some(req),
        _ => None,
    }
}

fn run_inner(args: Vec<String>) -> Result<i32, CliError> {
    let (mut globals, mut rest) = cli::parse_globals(&args)?;
    if globals.debug {
        // `create_clidriver` and `_handle_top_level_args` both run
        // `_set_logging`.
        crate::debuglog::enable();
        for _ in 0..2 {
            crate::debuglog::debug(
                "awscli.clidriver",
                &format!("CLI version: {}", version_string()),
            );
            crate::debuglog::debug(
                "awscli.clidriver",
                &format!(
                    "Arguments entered to CLI: {}",
                    crate::debuglog::list_repr(&args)
                ),
            );
        }
    }
    let mut session = new_session(&globals)?;
    let mut shadow: Vec<Vec<String>> = Vec::new();
    let mut proxied: Option<Vec<String>> = None;
    let mut depth = 0;
    let command = loop {
        match cli::parse_command_shadowed(&rest, &globals, &session, &shadow, proxied.as_deref())? {
            Command::Alias(cli::alias::Expansion::Dispatch {
                globals: new_globals,
                rest: new_rest,
                shadow: new_shadow,
                proxied: new_proxied,
            }) => {
                depth += 1;
                if depth > MAX_ALIAS_DEPTH {
                    return Err(CliError::new(
                        ExitCode::General,
                        render_general_error("maximum recursion depth exceeded"),
                    ));
                }
                globals = *new_globals;
                session = new_session(&globals)?;
                rest = new_rest;
                shadow.extend(new_shadow);
                if new_proxied.is_some() {
                    proxied = new_proxied;
                }
            }
            Command::Alias(cli::alias::Expansion::External(command)) => {
                return Ok(run_external_alias(&command));
            }
            command => break cli::driver::aliased_help(command, proxied.as_deref(), globals.help),
        }
    };
    if let Some(p) = proxied {
        session.set_proxied(p);
    }
    run_command(globals, session, command).map(|code| code as i32)
}

/// `ExternalAliasCommand.__call__`: `subprocess.call(command,
/// shell=True)`, whose return code is the exit status.
fn run_external_alias(command: &str) -> i32 {
    crate::output::stdout::flush();
    match Process::new("/bin/sh").arg("-c").arg(command).status() {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            match (status.code(), status.signal()) {
                (Some(code), _) => code,
                (None, Some(signal)) => (256 - signal) & 0xff,
                _ => 255,
            }
        }
        Err(e) => {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(render_general_error(&e.to_string()).as_bytes());
            255
        }
    }
}

fn run_command(
    globals: GlobalArgs,
    session: Session,
    command: Command,
) -> Result<ExitCode, CliError> {
    let ctx = Context {
        globals,
        session,
        runtime: LazyRuntime::default(),
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
        Command::Custom { path, args } => {
            match crate::help::custom_request(&path, &args, ctx.globals.help) {
                Some(req) => crate::help::run(&ctx, &req),
                None => crate::commands::run(&ctx, &path, &args),
            }
        }
        Command::Alias(_) => unreachable!("expanded by run_inner"),
    }
}

fn version_string() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    let (system, release) = if unsafe { libc::uname(&mut uts) } == 0 {
        let field = |f: &[libc::c_char]| {
            unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        (field(&uts.sysname), field(&uts.release))
    } else {
        (std::env::consts::OS.to_string(), String::new())
    };
    format!(
        "awsr/{} (aws-cli/{} compatible) {system}/{release} {}",
        env!("CARGO_PKG_VERSION"),
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

/// How a customization creates its client (`session.create_client(...,
/// region_name=..., endpoint_url=...)`) when it differs from the command's.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientConfig {
    /// `region_name` instead of `--region`.
    pub region: Option<Option<String>>,
    /// `endpoint_url=None` although `--endpoint-url` was given (the
    /// configured endpoint URL still applies).
    pub without_endpoint_url: bool,
    /// Which client object of the command this is: each has its own
    /// S3 region redirect cache.
    pub id: u32,
}

pub(crate) struct Caller<'a> {
    ctx: &'a Context,
    model: &'a ServiceModel,
    operation: Operation<'a>,
    builtins: Builtins,
    parse_options: ParseOptions,
    client: ClientConfig,
    /// The client's region.
    region: Option<String>,
    endpoint_url: Option<String>,
}

impl<'a> Caller<'a> {
    pub(crate) fn new(
        ctx: &'a Context,
        model: &'a ServiceModel,
        operation_name: &str,
    ) -> Result<Caller<'a>> {
        Caller::with_config(ctx, model, operation_name, ClientConfig::default())
    }

    /// [`Caller::new`] for a client created with `client`.
    pub(crate) fn with_config(
        ctx: &'a Context,
        model: &'a ServiceModel,
        operation_name: &str,
        client: ClientConfig,
    ) -> Result<Caller<'a>> {
        let operation = model
            .operation(operation_name)
            .ok_or_else(|| anyhow::anyhow!("unknown operation {operation_name}"))?;
        let settings = ctx.session.client_settings()?;
        let config = ctx.session.config();
        let client_region = match &client.region {
            Some(r) => r.clone(),
            None => settings.region.clone(),
        };
        if let Some(r) = &client_region {
            crate::session::validate_region_name(r)?;
        }
        let endpoint_url = match client.without_endpoint_url {
            true => None,
            false => ctx.globals.endpoint_url.clone(),
        }
        .or_else(|| config.configured_endpoint_url(&model.name).ok().flatten());
        let region = match client_region.clone() {
            Some(r) => Some(r),
            None => default_region(model, endpoint_url.is_some())?,
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
            endpoint_url: endpoint_url.clone(),
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
            client,
            region: client_region,
            endpoint_url,
        })
    }

    /// `S3RegionRedirectorv2.redirect_from_cache`: the region learned for
    /// the bucket replaces `AWS::Region`.
    fn builtins_for(&self, params: &Value) -> Builtins {
        let mut builtins = self.builtins.clone();
        if self.model.name == "s3"
            && let Some(bucket) = params.get("Bucket").and_then(Value::as_str)
            && let Some(region) = self.ctx.session.s3_redirect_region(self.client.id, bucket)
        {
            builtins.region = Some(region);
        }
        builtins
    }

    /// `S3RegionRedirectorv2.redirect_from_error`: the endpoint of the
    /// bucket's region when `response` says the bucket is in another one.
    fn redirect(
        &self,
        params: &Value,
        response: &protocol::HttpResponse,
    ) -> Result<Option<endpoint::Endpoint>> {
        if self.model.name != "s3" || response.status < 300 {
            return Ok(None);
        }
        let Some(bucket) = params.get("Bucket").and_then(Value::as_str) else {
            return Ok(None);
        };
        // `ArnParser.is_arn`.
        if bucket.starts_with("arn:") && bucket.splitn(6, ':').count() >= 6 {
            return Ok(None);
        }
        let error =
            protocol::parse_error(self.model, &self.operation, response, &self.parse_options)?;
        let parsed = &error.parsed;
        let code = parsed
            .get("Error")
            .and_then(|e| e.get("Code"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let header = |name: &str| {
            response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        let op = self.operation.name;
        let is_redirect = (matches!(code, "301" | "400") && op == "HeadObject")
            || (code == "AuthorizationHeaderMalformed"
                && parsed.get("Error").and_then(|e| e.get("Region")).is_some())
            || code == "PermanentRedirect"
            || (matches!(code, "301" | "400")
                && op == "HeadBucket"
                && header("x-amz-bucket-region").is_some())
            || matches!(response.status, 301 | 302 | 307)
            || (code == "IllegalLocationConstraintException" && op != "CreateBucket");
        if !is_redirect {
            return Ok(None);
        }
        // `get_bucket_region`.
        let region = match header("x-amz-bucket-region") {
            Some(r) => Some(r),
            None => match parsed
                .get("Error")
                .and_then(|e| e.get("Region"))
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
            {
                Some(r) => Some(r.to_string()),
                None => {
                    let mut head = crate::value::Map::new();
                    head.insert("Bucket".to_string(), Value::str(bucket));
                    let caller = Caller::with_config(
                        self.ctx,
                        self.model,
                        "HeadBucket",
                        self.client.clone(),
                    )?;
                    let headers = match caller.call(&Value::Map(head))? {
                        CallResult::Ok(v) => v,
                        CallResult::ServiceError(info) => info.parsed,
                    };
                    headers
                        .get("ResponseMetadata")
                        .and_then(|m| m.get("HTTPHeaders"))
                        .and_then(|h| h.get("x-amz-bucket-region"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            },
        };
        let Some(region) = region else {
            return Ok(None);
        };
        crate::session::validate_region_name(&region)?;
        self.ctx
            .session
            .set_s3_redirect_region(self.client.id, bucket, &region);
        let (_, _, endpoint) = self.prepare(params)?;
        Ok(Some(endpoint))
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

        let mut builtins = self.builtins_for(&params);
        if self.model.name == "s3" {
            endpoint::customize_s3_builtins(&mut builtins, self.operation.name, &params);
        }
        let use_ssl = !self
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
        let original_params = params.clone();
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
        let region = self.region.clone();
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
        let redirect = |response: &protocol::HttpResponse| {
            self.redirect(&original_params, response).map_err(|e| {
                match e.downcast::<SessionError>() {
                    Ok(se) => se,
                    Err(e) => SessionError::new(crate::session::ErrorKind::General, e.to_string()),
                }
            })
        };
        let outcome = block_on(
            &ctx.runtime,
            ctx.session.send_call(
                self.model,
                &self.operation,
                &endpoint,
                &request,
                &adjust,
                self.client.region.as_ref(),
                &redirect,
            ),
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
    let model = ServiceModel::load(&inv.service).map_err(|e| ctx.general_error(&e))?;
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
                let service_error: std::cell::RefCell<Option<Box<ClientErrorInfo>>> =
                    std::cell::RefCell::new(None);
                let call = |params: &Value| -> Result<Value> {
                    match caller.call(params)? {
                        CallResult::Ok(v) => Ok(v),
                        CallResult::ServiceError(info) => {
                            *service_error.borrow_mut() = Some(info);
                            Err(anyhow::anyhow!("service error"))
                        }
                    }
                };
                let iterator = paginate::PageIterator::new(
                    paginator,
                    &inv.params,
                    &inv.pagination,
                    paginate::limit_key_type(&operation, paginator),
                )
                .map_err(|e| ctx.general_error(&e))?;
                let pages = paginate::LazyPages {
                    iterator,
                    call: Box::new(call),
                };
                return display_response_mapped(
                    ctx,
                    &inv.operation,
                    Response::Lazy(Box::new(pages)),
                    b"",
                    &|e| match service_error.borrow().as_ref() {
                        Some(info) => client_error(ctx, info),
                        None => ctx.general_error(e),
                    },
                )
                .map(|()| ExitCode::Success);
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
    response: Response<'_>,
) -> Result<(), CliError> {
    display_response_then(ctx, command_name, response, b"")
}

/// [`display_response`] with `suffix` written to the same stream after
/// the formatter.
pub(crate) fn display_response_then(
    ctx: &Context,
    command_name: &str,
    response: Response<'_>,
    suffix: &[u8],
) -> Result<(), CliError> {
    display_response_mapped(ctx, command_name, response, suffix, &|e| {
        ctx.general_error(e)
    })
}

/// [`display_response_then`] with `map_error` turning a formatter error
/// (raised after the output written so far) into the CLI error.
pub(crate) fn display_response_mapped(
    ctx: &Context,
    command_name: &str,
    response: Response<'_>,
    suffix: &[u8],
    map_error: &dyn Fn(&anyhow::Error) -> CliError,
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
    if result.is_ok() {
        buf.extend_from_slice(suffix);
    }
    write_stdout(ctx, &buf)?;
    if let Err(e) = result {
        return Err(map_error(&e));
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
    let model = ServiceModel::load(&inv.service).map_err(|e| ctx.general_error(&e))?;
    let config = model
        .waiters()
        .and_then(|w| w.get(&inv.waiter))
        .and_then(|w| w.as_object())
        .map(crate::bjson::Obj::to_map)
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
        .block_on(waiter::wait(&inv.waiter, &config, &inv.params, &mut poll))
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
