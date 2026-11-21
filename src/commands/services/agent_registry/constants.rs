//! `awscli/customizations/agentregistry/constants.py` (the help texts are
//! in the vendored help data).

pub(crate) const SERVICE: &str = "agent-registry";

/// `DATA_PLANE_OPERATIONS`.
pub(crate) const DATA_PLANE_OPERATIONS: [&str; 3] = [
    "list-discoverable-registry-records",
    "search-discoverable-registry-records",
    "batch-get-discoverable-registry-record",
];

/// `DATA_PLANE_API_OPERATIONS`.
pub(crate) const DATA_PLANE_API_OPERATIONS: [&str; 3] = [
    "ListDiscoverableRegistryRecords",
    "SearchDiscoverableRegistryRecords",
    "BatchGetDiscoverableRegistryRecord",
];

pub(crate) const TOKEN_ENV_VAR: &str = "AWS_BEARER_TOKEN_AGENT_REGISTRY";
pub(crate) const BEARER_SCHEME: &str = "httpBearerAuth";

pub(crate) const PRM_PATH: &str =
    "/.well-known/oauth-protected-resource/registry/{registry_id}/mcp";
pub(crate) const OIDC_METADATA_PATH: &str = "/.well-known/openid-configuration";
pub(crate) const HTTP_TIMEOUT: u64 = 15;
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_PRINTED_TEXT: usize = 300;
pub(crate) const EXPIRY_SKEW_SECONDS: i64 = 120;

pub(crate) const CALLBACK_HOST: &str = "127.0.0.1";
pub(crate) const DEFAULT_CALLBACK_PORT: u16 = 8484;
pub(crate) const CALLBACK_PORT_ENV_VAR: &str = "AWS_AGENT_REGISTRY_CALLBACK_PORT";

pub(crate) const WANTED_SCOPES: [&str; 4] = ["openid", "profile", "email", "offline_access"];
