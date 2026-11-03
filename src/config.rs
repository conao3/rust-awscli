//! Session configuration: global options, environment variables, and the
//! shared config files, resolved the way botocore's session and the AWS CLI
//! do (`botocore/configprovider.py`, `botocore/configloader.py`,
//! `botocore/session.py`, `awscli/clidriver.py`).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::cli::GlobalArgs;
use crate::session::error::{ErrorKind, SessionError, py_int};

pub mod ini;
#[cfg(test)]
mod tests;

use ini::{FullConfig, IniValue, LoadError, Section};

/// Read access to resolved configuration variables (e.g. `region`,
/// `output`, `cli_binary_format`, `cli_timestamp_format`, `cli_pager`,
/// `retry_mode`, `max_attempts`, `ca_bundle`, `cli_error_format`), in the
/// AWS CLI's precedence (command line, environment, config file, default).
pub trait ConfigLookup {
    fn config_variable(&self, name: &str) -> Option<String>;
}

/// A fixed map, for tests.
impl ConfigLookup for HashMap<String, String> {
    fn config_variable(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

/// Where a chain looks in the scoped config: a top-level key, or a key
/// of a nested section (`('s3', 'addressing_style')`).
#[derive(Debug, Clone, Copy)]
enum ConfigKey<'a> {
    Key(&'a str),
    Nested(&'a str, &'a str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Conversion {
    None,
    Int,
    Bool,
}

/// One row of `BOTOCORE_DEFAUT_SESSION_VARIABLES` /
/// `DEFAULT_S3_CONFIG_VARS`: (instance name, env vars, config keys,
/// default, conversion).
struct Var {
    name: &'static str,
    instance: bool,
    env: &'static [&'static str],
    config: &'static [ConfigKey<'static>],
    default: Option<&'static str>,
    conversion: Conversion,
}

const fn var(
    name: &'static str,
    env: &'static [&'static str],
    config: &'static [ConfigKey<'static>],
    default: Option<&'static str>,
    conversion: Conversion,
) -> Var {
    Var {
        name,
        instance: true,
        env,
        config,
        default,
        conversion,
    }
}

use ConfigKey::{Key, Nested};
use Conversion::{Bool, Int, None as Raw};

/// `BOTOCORE_DEFAUT_SESSION_VARIABLES` (`botocore/configprovider.py`).
/// `region` is replaced by the AWS CLI's chain (`_construct_cli_region_chain`).
static SESSION_VARIABLES: &[Var] = &[
    var(
        "profile",
        &["AWS_PROFILE", "AWS_DEFAULT_PROFILE"],
        &[],
        None,
        Raw,
    ),
    var(
        "data_path",
        &["AWS_DATA_PATH"],
        &[Key("data_path")],
        None,
        Raw,
    ),
    var(
        "config_file",
        &["AWS_CONFIG_FILE"],
        &[],
        Some("~/.aws/config"),
        Raw,
    ),
    var(
        "ca_bundle",
        &["AWS_CA_BUNDLE"],
        &[Key("ca_bundle")],
        None,
        Raw,
    ),
    var(
        "credentials_file",
        &["AWS_SHARED_CREDENTIALS_FILE"],
        &[],
        Some("~/.aws/credentials"),
        Raw,
    ),
    var(
        "metadata_service_timeout",
        &["AWS_METADATA_SERVICE_TIMEOUT"],
        &[Key("metadata_service_timeout")],
        Some("1"),
        Int,
    ),
    var(
        "metadata_service_num_attempts",
        &["AWS_METADATA_SERVICE_NUM_ATTEMPTS"],
        &[Key("metadata_service_num_attempts")],
        Some("1"),
        Int,
    ),
    var(
        "ec2_metadata_service_endpoint",
        &["AWS_EC2_METADATA_SERVICE_ENDPOINT"],
        &[Key("ec2_metadata_service_endpoint")],
        None,
        Raw,
    ),
    var(
        "ec2_metadata_service_endpoint_mode",
        &["AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE"],
        &[Key("ec2_metadata_service_endpoint_mode")],
        None,
        Raw,
    ),
    var(
        "ec2_metadata_v1_disabled",
        &["AWS_EC2_METADATA_V1_DISABLED"],
        &[Key("ec2_metadata_v1_disabled")],
        Some("false"),
        Bool,
    ),
    var(
        "imds_use_ipv6",
        &["AWS_IMDS_USE_IPV6"],
        &[Key("imds_use_ipv6")],
        Some("false"),
        Bool,
    ),
    var(
        "use_dualstack_endpoint",
        &["AWS_USE_DUALSTACK_ENDPOINT"],
        &[Key("use_dualstack_endpoint")],
        None,
        Bool,
    ),
    var(
        "use_fips_endpoint",
        &["AWS_USE_FIPS_ENDPOINT"],
        &[Key("use_fips_endpoint")],
        None,
        Bool,
    ),
    var(
        "ignore_configured_endpoint_urls",
        &["AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"],
        &[Key("ignore_configured_endpoint_urls")],
        None,
        Bool,
    ),
    var(
        "parameter_validation",
        &[],
        &[Key("parameter_validation")],
        Some("true"),
        Raw,
    ),
    var(
        "csm_enabled",
        &["AWS_CSM_ENABLED"],
        &[Key("csm_enabled")],
        Some("false"),
        Bool,
    ),
    var(
        "csm_host",
        &["AWS_CSM_HOST"],
        &[Key("csm_host")],
        Some("127.0.0.1"),
        Raw,
    ),
    var(
        "csm_port",
        &["AWS_CSM_PORT"],
        &[Key("csm_port")],
        Some("31000"),
        Int,
    ),
    var(
        "csm_client_id",
        &["AWS_CSM_CLIENT_ID"],
        &[Key("csm_client_id")],
        Some(""),
        Raw,
    ),
    var(
        "endpoint_discovery_enabled",
        &["AWS_ENDPOINT_DISCOVERY_ENABLED"],
        &[Key("endpoint_discovery_enabled")],
        Some("auto"),
        Raw,
    ),
    var(
        "retry_mode",
        &["AWS_RETRY_MODE"],
        &[Key("retry_mode")],
        Some("standard"),
        Raw,
    ),
    var(
        "max_attempts",
        &["AWS_MAX_ATTEMPTS"],
        &[Key("max_attempts")],
        Some("3"),
        Int,
    ),
    var(
        "user_agent_appid",
        &["AWS_SDK_UA_APP_ID"],
        &[Key("sdk_ua_app_id")],
        None,
        Raw,
    ),
    var(
        "request_min_compression_size_bytes",
        &["AWS_REQUEST_MIN_COMPRESSION_SIZE_BYTES"],
        &[Key("request_min_compression_size_bytes")],
        Some("10240"),
        Raw,
    ),
    var(
        "disable_request_compression",
        &["AWS_DISABLE_REQUEST_COMPRESSION"],
        &[Key("disable_request_compression")],
        Some("false"),
        Bool,
    ),
    var(
        "sigv4a_signing_region_set",
        &["AWS_SIGV4A_SIGNING_REGION_SET"],
        &[Key("sigv4a_signing_region_set")],
        None,
        Raw,
    ),
    var(
        "request_checksum_calculation",
        &["AWS_REQUEST_CHECKSUM_CALCULATION"],
        &[Key("request_checksum_calculation")],
        Some("when_supported"),
        Raw,
    ),
    var(
        "response_checksum_validation",
        &["AWS_RESPONSE_CHECKSUM_VALIDATION"],
        &[Key("response_checksum_validation")],
        Some("when_supported"),
        Raw,
    ),
    var(
        "account_id_endpoint_mode",
        &["AWS_ACCOUNT_ID_ENDPOINT_MODE"],
        &[Key("account_id_endpoint_mode")],
        Some("preferred"),
        Raw,
    ),
    var(
        "disable_host_prefix_injection",
        &["AWS_DISABLE_HOST_PREFIX_INJECTION"],
        &[Key("disable_host_prefix_injection")],
        None,
        Bool,
    ),
    var(
        "auth_scheme_preference",
        &["AWS_AUTH_SCHEME_PREFERENCE"],
        &[Key("auth_scheme_preference")],
        None,
        Raw,
    ),
    var(
        "s3_disable_express_session_auth",
        &["AWS_S3_DISABLE_EXPRESS_SESSION_AUTH"],
        &[Key("s3_disable_express_session_auth")],
        None,
        Raw,
    ),
];

/// `DEFAULT_S3_CONFIG_VARS`, looked up as `s3.<name>`.
static S3_VARIABLES: &[Var] = &[
    Var {
        name: "addressing_style",
        instance: false,
        env: &[],
        config: &[Nested("s3", "addressing_style")],
        default: None,
        conversion: Raw,
    },
    Var {
        name: "use_accelerate_endpoint",
        instance: false,
        env: &[],
        config: &[Nested("s3", "use_accelerate_endpoint")],
        default: None,
        conversion: Bool,
    },
    Var {
        name: "use_dualstack_endpoint",
        instance: false,
        env: &[],
        config: &[Nested("s3", "use_dualstack_endpoint")],
        default: None,
        conversion: Bool,
    },
    Var {
        name: "payload_signing_enabled",
        instance: false,
        env: &[],
        config: &[Nested("s3", "payload_signing_enabled")],
        default: None,
        conversion: Bool,
    },
    Var {
        name: "use_arn_region",
        instance: false,
        env: &["AWS_S3_USE_ARN_REGION"],
        config: &[Key("s3_use_arn_region"), Nested("s3", "use_arn_region")],
        default: None,
        conversion: Bool,
    },
    Var {
        name: "s3_disable_multiregion_access_points",
        instance: false,
        env: &["AWS_S3_DISABLE_MULTIREGION_ACCESS_POINTS"],
        config: &[Nested("s3", "s3_disable_multiregion_access_points")],
        default: None,
        conversion: Bool,
    },
];

/// `DEFAULT_PROXIES_CONFIG_VARS`, looked up as `proxies_config.<name>`.
static PROXIES_VARIABLES: &[Var] = &[
    Var {
        name: "proxy_ca_bundle",
        instance: false,
        env: &[],
        config: &[Key("proxy_ca_bundle")],
        default: None,
        conversion: Raw,
    },
    Var {
        name: "proxy_client_cert",
        instance: false,
        env: &[],
        config: &[Key("proxy_client_cert")],
        default: None,
        conversion: Raw,
    },
    Var {
        name: "proxy_use_forwarding_for_https",
        instance: false,
        env: &[],
        config: &[Key("proxy_use_forwarding_for_https")],
        default: None,
        conversion: Bool,
    },
];

/// `utils.ensure_boolean` for a string.
pub fn ensure_boolean(value: &str) -> bool {
    value.to_lowercase() == "true"
}

/// `os.path.expandvars` + `os.path.expanduser` with the process
/// environment snapshot of a `Config`.
fn env_lookup(env: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
    move |k| env.get(k).cloned()
}

/// The resolved configuration of one CLI invocation: a snapshot of the
/// environment, the session instance variables set from the global
/// options (`--profile`, `--region`), the global options themselves, and
/// the merged config and credentials files (`Session.full_config`).
#[derive(Debug)]
pub struct Config {
    env: HashMap<String, String>,
    instance: HashMap<String, String>,
    globals: GlobalArgs,
    full: FullConfig,
    imds_region: OnceLock<Option<String>>,
}

impl Config {
    /// Build the configuration from the process environment.
    pub fn new(globals: &GlobalArgs) -> Result<Config, SessionError> {
        Config::with_env(globals, std::env::vars().collect())
    }

    /// Build the configuration from `env`. Loading the files fails with
    /// `ConfigParseError` the way `Session.full_config` does (the CLI hits
    /// it in the first `session-initialized` handler).
    pub fn with_env(
        globals: &GlobalArgs,
        env: HashMap<String, String>,
    ) -> Result<Config, SessionError> {
        let mut instance = HashMap::new();
        // `CLIDriver._handle_top_level_args`: only truthy values are set.
        if let Some(p) = globals.profile.as_ref().filter(|p| !p.is_empty()) {
            instance.insert("profile".to_string(), p.clone());
        }
        if let Some(r) = globals.region.as_ref().filter(|r| !r.is_empty()) {
            instance.insert("region".to_string(), r.clone());
        }
        let mut config = Config {
            env,
            instance,
            globals: globals.clone(),
            full: FullConfig::default(),
            imds_region: OnceLock::new(),
        };
        config.full = config.load_full_config()?;
        Ok(config)
    }

    /// `Session.full_config`: the config file's profile map, with the
    /// credentials file's sections merged into the profiles.
    fn load_full_config(&self) -> Result<FullConfig, SessionError> {
        let lookup = env_lookup(&self.env);
        let config_file = self.chain_raw("config_file").unwrap_or_default();
        let config_path = ini::expand_path(&config_file, &lookup);
        let mut full = match ini::load_config(&config_path) {
            Ok(full) => full,
            Err(LoadError::NotFound(_)) => FullConfig::default(),
            Err(LoadError::Parse(path)) => return Err(SessionError::config_parse(&path)),
        };
        let cred_file = self.chain_raw("credentials_file").unwrap_or_default();
        let cred_path = ini::expand_path(&cred_file, &lookup);
        match ini::raw_config_parse(&cred_path) {
            Ok(sections) => {
                for (profile, vars) in sections {
                    match full.profiles.get_mut(&profile) {
                        Some(existing) => existing.extend(vars),
                        None => {
                            full.profiles.insert(profile, vars);
                        }
                    }
                }
            }
            Err(LoadError::NotFound(_)) => {}
            Err(LoadError::Parse(path)) => return Err(SessionError::config_parse(&path)),
        }
        Ok(full)
    }

    pub fn env(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    pub fn env_map(&self) -> &HashMap<String, String> {
        &self.env
    }

    pub fn globals(&self) -> &GlobalArgs {
        &self.globals
    }

    pub fn full_config(&self) -> &FullConfig {
        &self.full
    }

    /// Whether `--profile` was given (`disable_env_vars` in
    /// `create_credential_resolver`).
    pub fn profile_explicitly_set(&self) -> bool {
        self.instance.contains_key("profile")
    }

    /// The `profile` session variable (`--profile`, `AWS_PROFILE`,
    /// `AWS_DEFAULT_PROFILE`), unset meaning `default`.
    pub fn profile(&self) -> Option<String> {
        self.chain_raw("profile")
    }

    /// The expanded config file path.
    pub fn config_file_path(&self) -> String {
        ini::expand_path(
            &self.chain_raw("config_file").unwrap_or_default(),
            &env_lookup(&self.env),
        )
    }

    /// The expanded shared credentials file path.
    pub fn credentials_file_path(&self) -> String {
        ini::expand_path(
            &self.chain_raw("credentials_file").unwrap_or_default(),
            &env_lookup(&self.env),
        )
    }

    /// `Session.get_scoped_config`.
    pub fn scoped_config(&self) -> Result<&Section, SessionError> {
        static EMPTY: OnceLock<Section> = OnceLock::new();
        match self.profile() {
            None => Ok(self
                .full
                .profiles
                .get("default")
                .unwrap_or_else(|| EMPTY.get_or_init(Section::new))),
            Some(name) => self
                .full
                .profiles
                .get(&name)
                .ok_or_else(|| SessionError::profile_not_found(&name)),
        }
    }

    /// A string value from the scoped config, `None` when the profile is
    /// missing or the value is a nested section.
    pub fn scoped_str(&self, key: &str) -> Option<String> {
        self.scoped_config()
            .ok()?
            .get(key)
            .and_then(IniValue::as_str)
            .map(str::to_string)
    }

    fn scoped_key(&self, key: ConfigKey<'_>) -> Result<Option<String>, SessionError> {
        let scoped = self.scoped_config()?;
        Ok(match key {
            Key(k) => scoped.get(k).and_then(IniValue::as_str).map(str::to_string),
            Nested(section, k) => scoped
                .get(section)
                .and_then(IniValue::as_nested)
                .and_then(|m| m.get(k))
                .cloned(),
        })
    }

    fn resolve_var(&self, v: &Var) -> Result<Option<String>, SessionError> {
        let raw = 'found: {
            if v.instance
                && let Some(x) = self.instance.get(v.name)
            {
                break 'found Some(x.clone());
            }
            for e in v.env {
                if let Some(x) = self.env.get(*e) {
                    break 'found Some(x.clone());
                }
            }
            for k in v.config {
                if let Some(x) = self.scoped_key(*k)? {
                    break 'found Some(x);
                }
            }
            v.default.map(str::to_string)
        };
        Ok(match (raw, v.conversion) {
            (None, _) => None,
            (Some(x), Raw) => Some(x),
            (Some(x), Int) => Some(py_int(&x)?.to_string()),
            (Some(x), Bool) => Some(ensure_boolean(&x).to_string()),
        })
    }

    /// A session variable chain without conversion, for chains that do
    /// not read the scoped config (`profile`, `config_file`,
    /// `credentials_file`).
    fn chain_raw(&self, name: &str) -> Option<String> {
        let v = SESSION_VARIABLES.iter().find(|v| v.name == name)?;
        if let Some(x) = self.instance.get(v.name) {
            return Some(x.clone());
        }
        for e in v.env {
            if let Some(x) = self.env.get(*e) {
                return Some(x.clone());
            }
        }
        v.default.map(str::to_string)
    }

    /// `Session.get_config_variable(name)`, with the AWS CLI's replaced
    /// chains (`CLIDriver._update_config_chain`) and the global options
    /// that take precedence where the CLI reads `parsed_globals` first.
    /// Errors are the ones Python raises while resolving: a missing
    /// profile, or a failed `int()` conversion. `s3.<name>` and
    /// `proxies_config.<name>` read the section providers.
    pub fn get(&self, name: &str) -> Result<Option<String>, SessionError> {
        match name {
            "region" => self.region(),
            "output" => {
                if let Some(o) = &self.globals.output {
                    return Ok(Some(o.clone()));
                }
                self.cli_chain(&["AWS_DEFAULT_OUTPUT"], "output", Some("json"))
            }
            "pager" | "cli_pager" => {
                if self.globals.no_cli_pager {
                    return Ok(None);
                }
                if let Some(p) = self.env.get("AWS_PAGER") {
                    return Ok(Some(p.clone()));
                }
                if let Some(p) = self.scoped_key(Key("cli_pager"))? {
                    return Ok(Some(p));
                }
                if let Some(p) = self.env.get("PAGER") {
                    return Ok(Some(p.clone()));
                }
                Ok(Some(DEFAULT_PAGER.to_string()))
            }
            "cli_binary_format" => {
                if let Some(f) = &self.globals.cli_binary_format {
                    return Ok(Some(f.clone()));
                }
                self.cli_chain(&[], "cli_binary_format", Some("base64"))
            }
            "cli_auto_prompt" => {
                if self.globals.no_cli_auto_prompt {
                    return Ok(Some("off".to_string()));
                }
                if self.globals.cli_auto_prompt {
                    return Ok(Some("on".to_string()));
                }
                self.cli_chain(&["AWS_CLI_AUTO_PROMPT"], "cli_auto_prompt", Some("off"))
            }
            "cli_help_output" => self.cli_chain(&[], "cli_help_output", Some("terminal")),
            "cli_error_format" => {
                if let Some(f) = self
                    .globals
                    .cli_error_format
                    .as_ref()
                    .filter(|f| !f.is_empty())
                {
                    return Ok(Some(f.clone()));
                }
                self.cli_chain(
                    &["AWS_CLI_ERROR_FORMAT"],
                    "cli_error_format",
                    Some("enhanced"),
                )
            }
            "cli_timestamp_format" => self.cli_chain(&[], "cli_timestamp_format", Some("iso8601")),
            "ca_bundle" => {
                if let Some(b) = &self.globals.ca_bundle {
                    return Ok(Some(b.clone()));
                }
                self.resolve_var(SESSION_VARIABLES.iter().find(|v| v.name == name).unwrap())
            }
            "cli_read_timeout" | "read_timeout" => Ok(Some(
                self.timeout_value(self.globals.cli_read_timeout.as_deref())?,
            )),
            "cli_connect_timeout" | "connect_timeout" => Ok(Some(
                self.timeout_value(self.globals.cli_connect_timeout.as_deref())?,
            )),
            _ => {
                if let Some(rest) = name.strip_prefix("s3.") {
                    if let Some(v) = S3_VARIABLES.iter().find(|v| v.name == rest) {
                        return self.resolve_var(v);
                    }
                    return self.scoped_key(Nested("s3", rest));
                }
                if let Some(rest) = name.strip_prefix("proxies_config.")
                    && let Some(v) = PROXIES_VARIABLES.iter().find(|v| v.name == rest)
                {
                    return self.resolve_var(v);
                }
                if let Some(v) = SESSION_VARIABLES.iter().find(|v| v.name == name) {
                    return self.resolve_var(v);
                }
                self.scoped_key(Key(name))
            }
        }
    }

    /// `_resolve_timeout`: the global option or `DEFAULT_TIMEOUT` (60),
    /// `int()`-converted; `0` means no timeout (returned as `"0"`).
    fn timeout_value(&self, arg: Option<&str>) -> Result<String, SessionError> {
        match arg {
            None => Ok("60".to_string()),
            Some(v) => Ok(py_int(v)?.to_string()),
        }
    }

    /// A chain of `EnvironmentProvider`s, a `ScopedConfigProvider`, and a
    /// `ConstantProvider` (the CLI's chains).
    fn cli_chain(
        &self,
        env: &[&str],
        key: &str,
        default: Option<&str>,
    ) -> Result<Option<String>, SessionError> {
        for e in env {
            if let Some(x) = self.env.get(*e) {
                return Ok(Some(x.clone()));
            }
        }
        if let Some(x) = self.scoped_key(Key(key))? {
            return Ok(Some(x));
        }
        Ok(default.map(str::to_string))
    }

    /// `_construct_cli_region_chain`: `--region`, `AWS_REGION`,
    /// `AWS_DEFAULT_REGION`, the scoped config, then IMDS.
    pub fn region(&self) -> Result<Option<String>, SessionError> {
        if let Some(r) = self.instance.get("region") {
            return Ok(Some(r.clone()));
        }
        for e in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
            if let Some(r) = self.env.get(e) {
                return Ok(Some(r.clone()));
            }
        }
        if let Some(r) = self.scoped_key(Key("region"))? {
            return Ok(Some(r));
        }
        Ok(self
            .imds_region
            .get_or_init(|| crate::session::imds::fetch_region(self))
            .clone())
    }

    /// An integer variable (`max_attempts`, `metadata_service_timeout`).
    pub fn get_int(&self, name: &str) -> Result<Option<i64>, SessionError> {
        Ok(match self.get(name)? {
            None => None,
            Some(v) => Some(py_int(&v)?),
        })
    }

    /// A boolean variable (converted with `ensure_boolean`).
    pub fn get_bool(&self, name: &str) -> Result<Option<bool>, SessionError> {
        Ok(self.get(name)?.map(|v| ensure_boolean(&v)))
    }

    /// `ConfiguredEndpointProvider.provide()` for the service whose client
    /// name (botocore service name) is `client_name`: the service-specific
    /// environment variable, `AWS_ENDPOINT_URL`, the `[services]` section
    /// entry, then the profile's `endpoint_url`. `None` when
    /// `ignore_configured_endpoint_urls` is true.
    pub fn configured_endpoint_url(
        &self,
        client_name: &str,
    ) -> Result<Option<String>, SessionError> {
        if self.get_bool("ignore_configured_endpoint_urls")? == Some(true) {
            return Ok(None);
        }
        let scoped = self.scoped_config()?;
        let service_id = snake_case_service_id(client_name);
        let env_name = format!("AWS_ENDPOINT_URL_{}", service_id.to_uppercase());
        if let Some(u) = self.env.get(&env_name).filter(|u| !u.is_empty()) {
            return Ok(Some(u.clone()));
        }
        if let Some(u) = self.env.get("AWS_ENDPOINT_URL").filter(|u| !u.is_empty()) {
            return Ok(Some(u.clone()));
        }
        if let Some(section_name) = scoped.get("services") {
            let section_name = section_name.as_str().unwrap_or_default();
            let services = self
                .full
                .services
                .get(section_name)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    SessionError::new(
                        ErrorKind::InvalidConfig,
                        format!(
                            "The profile is configured to use the services section but the \
                             \"{section_name}\" services configuration does not exist."
                        ),
                    )
                })?;
            if let Some(u) = services
                .get(&service_id.to_lowercase())
                .and_then(IniValue::as_nested)
                .and_then(|m| m.get("endpoint_url"))
                .filter(|u| !u.is_empty())
            {
                return Ok(Some(u.clone()));
            }
        }
        Ok(scoped
            .get("endpoint_url")
            .and_then(IniValue::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string))
    }
}

/// The CLI's `default_pager` on POSIX (`awscli/compat.py`).
const DEFAULT_PAGER: &str = "less";

/// `ConfiguredEndpointProvider._get_snake_case_service_id`, with
/// `SERVICE_NAME_ALIASES` and `CLIENT_NAME_TO_HYPHENIZED_SERVICE_ID_OVERRIDES`
/// (`botocore/utils.py`).
pub fn snake_case_service_id(client_name: &str) -> String {
    let client_name = match client_name {
        "runtime.sagemaker" => "sagemaker-runtime",
        other => other,
    };
    let hyphenized = match client_name {
        "apigateway" => "api-gateway",
        "application-autoscaling" => "application-auto-scaling",
        "appmesh" => "app-mesh",
        "autoscaling" => "auto-scaling",
        "autoscaling-plans" => "auto-scaling-plans",
        "ce" => "cost-explorer",
        "cloudhsmv2" => "cloudhsm-v2",
        "cloudsearchdomain" => "cloudsearch-domain",
        "cognito-idp" => "cognito-identity-provider",
        "config" => "config-service",
        "cur" => "cost-and-usage-report-service",
        "datapipeline" => "data-pipeline",
        "directconnect" => "direct-connect",
        "devicefarm" => "device-farm",
        "discovery" => "application-discovery-service",
        "dms" => "database-migration-service",
        "ds" => "directory-service",
        "ds-data" => "directory-service-data",
        "dynamodbstreams" => "dynamodb-streams",
        "elasticbeanstalk" => "elastic-beanstalk",
        "elb" => "elastic-load-balancing",
        "elbv2" => "elastic-load-balancing-v2",
        "es" => "elasticsearch-service",
        "events" => "eventbridge",
        "globalaccelerator" => "global-accelerator",
        "iot-data" => "iot-data-plane",
        "iot-jobs-data" => "iot-jobs-data-plane",
        "iotwireless" => "iot-wireless",
        "kinesisanalytics" => "kinesis-analytics",
        "kinesisanalyticsv2" => "kinesis-analytics-v2",
        "kinesisvideo" => "kinesis-video",
        "lex-models" => "lex-model-building-service",
        "lexv2-models" => "lex-models-v2",
        "lex-runtime" => "lex-runtime-service",
        "lexv2-runtime" => "lex-runtime-v2",
        "logs" => "cloudwatch-logs",
        "machinelearning" => "machine-learning",
        "marketplacecommerceanalytics" => "marketplace-commerce-analytics",
        "marketplace-entitlement" => "marketplace-entitlement-service",
        "meteringmarketplace" => "marketplace-metering",
        "mgh" => "migration-hub",
        "resourcegroupstaggingapi" => "resource-groups-tagging-api",
        "route53" => "route-53",
        "route53domains" => "route-53-domains",
        "s3control" => "s3-control",
        "sdb" => "simpledb",
        "secretsmanager" => "secrets-manager",
        "serverlessrepo" => "serverlessapplicationrepository",
        "servicecatalog" => "service-catalog",
        "servicecatalog-appregistry" => "service-catalog-appregistry",
        "stepfunctions" => "sfn",
        "storagegateway" => "storage-gateway",
        other => other,
    };
    hyphenized.replace('-', "_")
}

impl ConfigLookup for Config {
    /// Resolved values; `None` when unset or when resolution raises (a
    /// missing profile is reported by `Session` when a client is made).
    fn config_variable(&self, name: &str) -> Option<String> {
        match self.get(name) {
            // `ErrorHandler._resolve_error_format` lowercases the value and
            // treats an empty one as unset.
            Ok(v) if name == "cli_error_format" => Some(
                v.filter(|f| !f.is_empty())
                    .map_or_else(|| "enhanced".to_string(), |f| f.to_lowercase()),
            ),
            Ok(v) => v,
            // `add_binary_formatter` and `add_timestamp_parser` fall back
            // to their defaults on `ProfileNotFound`.
            Err(_) => match name {
                "output" => Some("json".to_string()),
                "cli_binary_format" => Some("base64".to_string()),
                "cli_timestamp_format" => Some("iso8601".to_string()),
                "cli_error_format" => Some("enhanced".to_string()),
                _ => None,
            },
        }
    }
}
