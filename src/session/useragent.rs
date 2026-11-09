//! The `User-Agent` header (`botocore/useragent.py` `UserAgentString`,
//! with the AWS CLI's session name, version, and extra components from
//! `awscli/clidriver.py` `_set_user_agent_for_session` and
//! `awscli/telemetry.py`). Feature ids (`m/...`) come from a Python
//! `set`, whose order varies between runs of the reference; here they are
//! in a fixed order.

/// `_USERAGENT_ALLOWED_CHARACTERS` (+ `#` when allowed).
pub fn sanitize(raw: &str, allow_hash: bool) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || "!$%&'*+-.^_`|~,".contains(c)
                || (allow_hash && c == '#')
            {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `UserAgentComponent.to_string`.
fn component(prefix: &str, name: &str, value: Option<&str>) -> String {
    let p = sanitize(prefix, true);
    let n = sanitize(name, false);
    match value.filter(|v| !v.is_empty()) {
        Some(v) => format!("{p}/{n}#{}", sanitize(v, true)),
        None => format!("{p}/{n}"),
    }
}

/// `_USERAGENT_FEATURE_MAPPINGS` entries used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    AgenticCallerClaudeCode,
    Paginator,
    Waiter,
    RetryModeStandard,
    RetryModeAdaptive,
    EndpointOverride,
    FlexibleChecksumsReqWhenSupported,
    FlexibleChecksumsResWhenSupported,
    FlexibleChecksumsReqWhenRequired,
    FlexibleChecksumsResWhenRequired,
    CredentialsCode,
    CredentialsEnvVars,
    CredentialsEnvVarsStsWebIdToken,
    CredentialsStsAssumeRole,
    CredentialsStsAssumeRoleWebId,
    CredentialsProfile,
    CredentialsProfileSourceProfile,
    CredentialsProfileNamedProvider,
    CredentialsProfileStsWebIdToken,
    CredentialsProfileSso,
    CredentialsSso,
    CredentialsProfileSsoLegacy,
    CredentialsSsoLegacy,
    CredentialsProfileProcess,
    CredentialsProcess,
    CredentialsBoto2ConfigFile,
    CredentialsHttp,
    CredentialsImds,
    CredentialsProfileLogin,
    CredentialsLogin,
    Sigv4aSigning,
    BearerServiceEnvVars,
    S3Transfer,
    S3TransferClassicExplicit,
    S3TransferClassicAuto,
    S3Redirect,
    ProtocolRpcV2Cbor,
    GzipRequestCompression,
    AccountIdModePreferred,
    AccountIdModeDisabled,
    AccountIdModeRequired,
    ResolvedAccountId,
    FlexibleChecksumsReqCrc32,
    FlexibleChecksumsReqCrc32c,
    FlexibleChecksumsReqCrc64,
    FlexibleChecksumsReqSha1,
    FlexibleChecksumsReqSha256,
    FlexibleChecksumsReqMd5,
    FlexibleChecksumsReqSha512,
    FlexibleChecksumsReqXxhash3,
    FlexibleChecksumsReqXxhash64,
    FlexibleChecksumsReqXxhash128,
    SsoLoginVanityUrl,
    LoginSameDevice,
    LoginCrossDevice,
}

impl Feature {
    pub fn code(&self) -> &'static str {
        match self {
            Feature::AgenticCallerClaudeCode => "AN",
            Feature::Paginator => "C",
            Feature::Waiter => "B",
            Feature::RetryModeStandard => "E",
            Feature::RetryModeAdaptive => "F",
            Feature::EndpointOverride => "N",
            Feature::FlexibleChecksumsReqWhenSupported => "Z",
            Feature::FlexibleChecksumsResWhenSupported => "b",
            Feature::FlexibleChecksumsReqWhenRequired => "a",
            Feature::FlexibleChecksumsResWhenRequired => "c",
            Feature::CredentialsCode => "e",
            Feature::CredentialsEnvVars => "g",
            Feature::CredentialsEnvVarsStsWebIdToken => "h",
            Feature::CredentialsStsAssumeRole => "i",
            Feature::CredentialsStsAssumeRoleWebId => "k",
            Feature::CredentialsProfile => "n",
            Feature::CredentialsProfileSourceProfile => "o",
            Feature::CredentialsProfileNamedProvider => "p",
            Feature::CredentialsProfileStsWebIdToken => "q",
            Feature::CredentialsProfileSso => "r",
            Feature::CredentialsSso => "s",
            Feature::CredentialsProfileSsoLegacy => "t",
            Feature::CredentialsSsoLegacy => "u",
            Feature::CredentialsProfileProcess => "v",
            Feature::CredentialsProcess => "w",
            Feature::CredentialsBoto2ConfigFile => "x",
            Feature::CredentialsHttp => "z",
            Feature::CredentialsImds => "0",
            Feature::CredentialsProfileLogin => "AC",
            Feature::CredentialsLogin => "AD",
            Feature::Sigv4aSigning => "S",
            Feature::BearerServiceEnvVars => "3",
            Feature::S3Transfer => "G",
            Feature::S3TransferClassicExplicit => "Ad",
            Feature::S3TransferClassicAuto => "Af",
            Feature::S3Redirect => "Ah",
            Feature::ProtocolRpcV2Cbor => "M",
            Feature::GzipRequestCompression => "L",
            Feature::AccountIdModePreferred => "P",
            Feature::AccountIdModeDisabled => "Q",
            Feature::AccountIdModeRequired => "R",
            Feature::ResolvedAccountId => "T",
            Feature::FlexibleChecksumsReqCrc32 => "U",
            Feature::FlexibleChecksumsReqCrc32c => "V",
            Feature::FlexibleChecksumsReqCrc64 => "W",
            Feature::FlexibleChecksumsReqSha1 => "X",
            Feature::FlexibleChecksumsReqSha256 => "Y",
            Feature::FlexibleChecksumsReqMd5 => "AE",
            Feature::FlexibleChecksumsReqSha512 => "AF",
            Feature::FlexibleChecksumsReqXxhash3 => "AG",
            Feature::FlexibleChecksumsReqXxhash64 => "AH",
            Feature::FlexibleChecksumsReqXxhash128 => "AI",
            Feature::SsoLoginVanityUrl => "AM",
            Feature::LoginSameDevice => "AA",
            Feature::LoginCrossDevice => "AB",
        }
    }
}

/// The inputs of one `User-Agent` string.
#[derive(Debug, Clone, Default)]
pub struct UserAgent {
    pub cli_version: String,
    pub execution_env: Option<String>,
    pub features: Vec<Feature>,
    pub retry_mode: Option<String>,
    pub app_id: Option<String>,
    pub session_id: String,
    /// `md/command#<service>.<operation>` (CLI names).
    pub command: Option<String>,
}

fn os_release() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
}

/// `awscli.utils._get_distribution` (`<id>.<major version>` from
/// `/etc/os-release`).
fn linux_distribution() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    let field = |k: &str| {
        text.lines().find_map(|l| {
            l.strip_prefix(&format!("{k}="))
                .map(|v| v.trim_matches('"').to_string())
        })
    };
    let id = field("ID")?;
    match field("VERSION_ID").and_then(|v| v.split('.').next().map(str::to_string)) {
        Some(major) if !major.is_empty() => Some(format!("{id}.{major}")),
        _ => Some(id),
    }
}

impl UserAgent {
    pub fn to_header(&self) -> String {
        let mut parts = vec![component("aws-cli", &self.cli_version, None)];
        parts[0] = format!("aws-cli/{}", sanitize(&self.cli_version, true));
        parts.push(component("md", "awsr", Some(env!("CARGO_PKG_VERSION"))));
        parts.push("ua/2.1".to_string());
        let (os, release) = match std::env::consts::OS {
            "linux" => ("linux", os_release()),
            "macos" => ("macos", None),
            "windows" => ("windows", None),
            other => (other, None),
        };
        parts.push(component("os", os, release.as_deref()));
        let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "arm64",
            (_, a) => a,
        };
        parts.push(component("md", "arch", Some(arch)));
        parts.push("lang/rust".to_string());
        if let Some(e) = self.execution_env.as_deref().filter(|e| !e.is_empty()) {
            parts.push(component("exec-env", e, None));
        }
        let mut features = self.features.clone();
        features.sort();
        features.dedup();
        if !features.is_empty() {
            let joined: Vec<&str> = features.iter().map(Feature::code).collect();
            parts.push(format!("m/{}", sanitize(&joined.join(","), true)));
        }
        if let Some(mode) = &self.retry_mode {
            parts.push(component("cfg", "retry-mode", Some(mode)));
        }
        if let Some(app) = self.app_id.as_deref().filter(|a| !a.is_empty()) {
            parts.push(format!("app/{}", sanitize(app, true)));
        }
        parts.push(component("md", "installer", Some("awsr")));
        parts.push(format!("sid/{}", self.session_id));
        if let Some(d) = linux_distribution() {
            parts.push(component("md", "distrib", Some(&d)));
        }
        parts.push(component("md", "prompt", Some("off")));
        if let Some(c) = &self.command {
            parts.push(component("md", "command", Some(c)));
        }
        parts.join(" ")
    }
}

/// The 12-hex-digit session id of the process (`telemetry.py`).
pub fn session_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_string())
}

static COMMAND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static REGISTERED: std::sync::Mutex<Vec<Feature>> = std::sync::Mutex::new(Vec::new());

/// `md/command#<service>.<operation>` of the session's user agent extra
/// (`add_command_lineage_to_user_agent_extra`).
pub fn set_command(command: String) {
    *COMMAND.lock().unwrap() = Some(command);
}

pub fn command() -> Option<String> {
    COMMAND.lock().unwrap().clone()
}

/// `register_feature_ids` of the credential providers, in the context the
/// command's clients are created in.
pub fn register(features: &[Feature]) {
    REGISTERED.lock().unwrap().extend_from_slice(features);
}

pub fn registered() -> Vec<Feature> {
    REGISTERED.lock().unwrap().clone()
}

static CREDENTIAL_CLIENT_CLAIMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the client being created is the command's first, whose
/// creation loads the credentials (`Session.create_client` calls
/// `get_credentials`): true only once.
pub fn claim_credential_client() -> bool {
    !CREDENTIAL_CLIENT_CLAIMED.swap(true, std::sync::atomic::Ordering::Relaxed)
}

static COMMAND_CREDENTIALS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `session.get_credentials()` outside of `create_client`: a load of the
/// credentials before any client is created registers the providers'
/// features in the command's context, so that no client is the one that
/// loaded them.
pub fn load_credentials_in_command() {
    if claim_credential_client() {
        COMMAND_CREDENTIALS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether the command's context carries the credential providers' features.
pub fn command_credentials() -> bool {
    COMMAND_CREDENTIALS.load(std::sync::atomic::Ordering::Relaxed)
}

static COMMAND_CONTEXT: std::sync::Mutex<Vec<Feature>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// The features of the context of the call this thread makes
    /// (`botocore.context` of `_make_api_call`).
    static CONTEXT: std::cell::RefCell<Vec<Feature>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Whether the client of the call is the one created while the
    /// credentials were loaded, whose features include the providers'.
    static CREDENTIAL_CLIENT: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// `register_feature_id` in the command's context, which every call and
/// thread of the command copies (`S3_TRANSFER`, the SSO and login flows).
pub fn register_command(features: &[Feature]) {
    COMMAND_CONTEXT.lock().unwrap().extend_from_slice(features);
}

/// `register_feature_id` in the context of the current call.
pub fn register_context(feature: Feature) {
    CONTEXT.with(|c| c.borrow_mut().push(feature));
}

/// The features of the current context: the command's (with the
/// credential providers' when the command loaded the credentials) and the
/// call's.
pub fn context() -> Vec<Feature> {
    let mut features = COMMAND_CONTEXT.lock().unwrap().clone();
    if command_credentials() {
        features.extend(registered());
    }
    CONTEXT.with(|c| features.extend_from_slice(&c.borrow()));
    features
}

/// Whether the current call's client carries the credential providers'
/// features.
pub fn credential_client() -> bool {
    CREDENTIAL_CLIENT.with(std::cell::Cell::get)
}

/// `start_as_current_context`: a copy of the current context with
/// `features` added, which the context it replaces gets back on drop.
pub struct ContextGuard {
    saved: Vec<Feature>,
    credential_client: bool,
}

impl ContextGuard {
    pub fn new(features: &[Feature]) -> ContextGuard {
        let saved = CONTEXT.with(|c| {
            let saved = c.borrow().clone();
            c.borrow_mut().extend_from_slice(features);
            saved
        });
        ContextGuard {
            saved,
            credential_client: credential_client(),
        }
    }

    /// The context of a call of a client that is not the one created
    /// while the credentials were loaded.
    pub fn without_credential_features(self) -> ContextGuard {
        CREDENTIAL_CLIENT.with(|c| c.set(false));
        self
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        let saved = std::mem::take(&mut self.saved);
        CONTEXT.with(|c| *c.borrow_mut() = saved);
        CREDENTIAL_CLIENT.with(|c| c.set(self.credential_client));
    }
}

/// `_register_checksum_algorithm_feature_id` of the algorithm `name`.
pub fn checksum_algorithm(name: &str) -> Option<Feature> {
    Some(match name.to_ascii_uppercase().as_str() {
        "CRC32" => Feature::FlexibleChecksumsReqCrc32,
        "CRC32C" => Feature::FlexibleChecksumsReqCrc32c,
        "CRC64NVME" => Feature::FlexibleChecksumsReqCrc64,
        "SHA1" => Feature::FlexibleChecksumsReqSha1,
        "SHA256" => Feature::FlexibleChecksumsReqSha256,
        "MD5" => Feature::FlexibleChecksumsReqMd5,
        "SHA512" => Feature::FlexibleChecksumsReqSha512,
        "XXHASH3" => Feature::FlexibleChecksumsReqXxhash3,
        "XXHASH64" => Feature::FlexibleChecksumsReqXxhash64,
        "XXHASH128" => Feature::FlexibleChecksumsReqXxhash128,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let ua = UserAgent {
            cli_version: "2.39.3".into(),
            features: vec![Feature::RetryModeStandard, Feature::CredentialsEnvVars],
            retry_mode: Some("standard".into()),
            session_id: "b9894c0f42e5".into(),
            command: Some("sts.get-caller-identity".into()),
            ..Default::default()
        }
        .to_header();
        assert!(ua.starts_with("aws-cli/2.39.3 md/awsr#"));
        assert!(ua.contains(" m/E,g cfg/retry-mode#standard "));
        assert!(ua.ends_with(" md/prompt#off md/command#sts.get-caller-identity"));
        assert_eq!(sanitize("a b/c#d", false), "a-b-c-d");
    }
}
