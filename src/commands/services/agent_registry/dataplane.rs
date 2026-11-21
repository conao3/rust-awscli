//! `awscli/customizations/agentregistry/dataplane.py`: the data plane
//! client of `list-skills` and `install-skill`, with their sign-in
//! arguments.

use super::basic::{ArgDef, Args};
use super::constants::{BEARER_SCHEME, SERVICE, TOKEN_ENV_VAR};
use super::providers::{SignIn, use_bearer_token};
use super::utils::{
    redact_logs_and_history, require_verified_tls, resolve_ca_bundle, resolve_data_plane_endpoint,
};
use super::{Error, Result};
use crate::commands::services::exec::Client;
use crate::driver::Context;
use crate::paginate::PaginationConfig;
use crate::value::{Map, Value};

/// `SKILL_RECORD_TYPE`.
const SKILL_RECORD_TYPE: &str = "SKILL";

/// `SKILL_RECORD_PAGE_SIZE`.
const SKILL_RECORD_PAGE_SIZE: i64 = 100;

/// `SIGN_IN_ARGUMENTS`.
pub(crate) fn sign_in_arguments() -> Vec<ArgDef> {
    vec![
        ArgDef::new("authorizer").choices(&["JWT", "SIGV4"]),
        ArgDef::new("client-id"),
        ArgDef::new("no-browser").flag(),
    ]
}

/// `DataPlane`: the client's context, the endpoint the call goes to, and
/// the region and CA bundle for other requests.
pub(crate) struct DataPlane {
    /// The context of a client created with a bearer token, or `None`
    /// for the command's own.
    own: Option<Context>,
    pub endpoint: String,
    pub region: String,
    pub ca_bundle: Option<String>,
}

impl DataPlane {
    pub(crate) fn context<'c>(&'c self, ctx: &'c Context) -> &'c Context {
        self.own.as_ref().unwrap_or(ctx)
    }

    /// `client`.
    pub(crate) fn client<'c>(&'c self, ctx: &'c Context) -> Result<Client<'c>> {
        Client::new(self.context(ctx), SERVICE).map_err(Error::Cli)
    }
}

/// The context of `session.create_client(...)` after `os.environ` has
/// `AWS_BEARER_TOKEN_AGENT_REGISTRY` (when `token` is given), with
/// `Config(auth_scheme_preference='httpBearerAuth')`.
fn bearer_context(ctx: &Context, token: Option<&str>) -> Result<Context> {
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    if let Some(t) = token {
        env.insert(TOKEN_ENV_VAR.to_string(), t.to_string());
    }
    // A preference set in code takes precedence over the environment and
    // the config file.
    env.insert(
        "AWS_AUTH_SCHEME_PREFERENCE".to_string(),
        BEARER_SCHEME.to_string(),
    );
    let session = crate::session::Session::with_env(&ctx.globals, env)
        .map_err(|e| Error::Cli(ctx.session_error(&e)))?;
    Ok(Context {
        globals: ctx.globals.clone(),
        session,
        runtime: crate::driver::LazyRuntime::default(),
        terminal: crate::driver::Terminal {
            stdout_is_tty: ctx.terminal.stdout_is_tty,
            width: ctx.terminal.width,
        },
    })
}

/// `create_data_plane_client(session, parsed_args, parsed_globals)`.
pub(crate) fn create_data_plane_client(ctx: &Context, args: &Args) -> Result<DataPlane> {
    let region = ctx
        .globals
        .region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| ctx.session.config().get("region").ok().flatten())
        .filter(|r| !r.is_empty());
    let Some(region) = region else {
        return Err(Error::Jwt(
            "A region is required. Pass --region or configure a default region.".to_string(),
        ));
    };
    let ca_bundle = resolve_ca_bundle(ctx);
    redact_logs_and_history(&[]);
    let jwt = args
        .str("authorizer")
        .is_some_and(|a| !a.is_empty() && a.to_uppercase() == "JWT");
    if jwt {
        require_verified_tls(&ctx.globals)?;
    }
    let endpoint = resolve_data_plane_endpoint(ctx, &region, jwt)?;
    let mut own = None;
    if jwt {
        let token = use_bearer_token(
            ctx,
            SignIn {
                endpoint: &endpoint,
                registry_id: &args.str("registry-id").unwrap_or_default(),
                client_id: args.str("client-id"),
                ca_bundle: ca_bundle.as_deref(),
                open_browser: !args.bool("no-browser"),
            },
        )?;
        own = Some(bearer_context(ctx, token.as_deref())?);
    }
    super::arm_unverified_tls_check(&ctx.globals);
    Ok(DataPlane {
        own,
        endpoint,
        region,
        ca_bundle,
    })
}

/// `skill_record_pages(client, registry_id)`: the `SKILL` records of
/// every page.
pub(crate) fn skill_record_pages(
    client: &Client<'_>,
    registry_id: &str,
) -> Result<Vec<Vec<Value>>> {
    let mut filter = Map::new();
    filter.insert("name".to_string(), Value::str("recordType"));
    filter.insert(
        "values".to_string(),
        Value::List(vec![Value::str(SKILL_RECORD_TYPE)]),
    );
    let mut params = Map::new();
    params.insert("registryId".to_string(), Value::str(registry_id));
    params.insert("filters".to_string(), Value::List(vec![Value::Map(filter)]));
    let pages = client
        .paginate_with(
            "ListDiscoverableRegistryRecords",
            &Value::Map(params),
            &PaginationConfig {
                page_size: Some(SKILL_RECORD_PAGE_SIZE),
                ..Default::default()
            },
        )
        .map_err(Error::Cli)?;
    Ok(pages
        .iter()
        .map(|page| match page.get("registryRecords") {
            Some(Value::List(records)) => records
                .iter()
                .filter(|r| {
                    matches!(r, Value::Map(_))
                        && r.get("recordType") == Some(&Value::str(SKILL_RECORD_TYPE))
                })
                .cloned()
                .collect(),
            _ => vec![],
        })
        .collect())
}
