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
    CredentialsEnvVars,
    CredentialsProfile,
    Sigv4aSigning,
    BearerServiceEnvVars,
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
            Feature::CredentialsEnvVars => "g",
            Feature::CredentialsProfile => "n",
            Feature::Sigv4aSigning => "S",
            Feature::BearerServiceEnvVars => "3",
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
pub(crate) fn linux_distribution() -> Option<String> {
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

/// A 12-hex-digit session id (`telemetry.py`).
pub fn new_session_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id[..12].to_string()
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
