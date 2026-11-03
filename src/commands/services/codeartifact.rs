//! `codeartifact login` (`awscli/customizations/codeartifact/login.py`).

use std::process::Command;

use chrono::{DateTime, Datelike, FixedOffset, Months, Timelike, Utc};

use super::basic::{self, ArgDef};
use super::exec::Client;
use crate::cli::argparse::NsVal;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::pyfmt::str_repr;
use crate::session::url::urlsplit;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "login" => login(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `CodeArtifactLogin.TOOL_MAP`: (tool, package format, namespace support).
const TOOLS: &[(&str, &str, bool)] = &[
    ("swift", "swift", true),
    ("nuget", "nuget", false),
    ("dotnet", "nuget", false),
    ("npm", "npm", true),
    ("pip", "pypi", false),
    ("twine", "pypi", false),
];

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "~".to_string())
}

fn expanduser(p: &str) -> String {
    match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", home()),
        _ => p.to_string(),
    }
}

/// `CodeArtifactLogin._run_main`.
fn login(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("tool")
            .choices(&TOOLS.iter().map(|t| t.0).collect::<Vec<_>>())
            .required(),
        ArgDef::new("domain").required(),
        ArgDef::new("domain-owner"),
        ArgDef::new("namespace"),
        ArgDef::new("duration-seconds").int(),
        ArgDef::new("repository").required(),
        ArgDef::new("endpoint-type"),
        ArgDef::new("dry-run").flag().default(NsVal::Bool(false)),
    ];
    let Some(a) = basic::parse(ctx, &["codeartifact", "login"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let tool = a.str("tool").unwrap_or_default().to_lowercase();
    let &(_, package_format, namespace_support) =
        TOOLS.iter().find(|t| t.0 == tool).expect("choice");
    let client = Client::new(ctx, "codeartifact")?;
    let domain = a.str("domain").unwrap_or_default();
    let repository = a.str("repository").unwrap_or_default();
    let mut params = Map::new();
    params.insert("domain".into(), Value::str(&domain));
    if let Some(o) = a.get("domain-owner").filter(|v| v.is_truthy()) {
        params.insert("domainOwner".into(), o.clone());
    }
    if let Some(d) = a.get("duration-seconds").filter(|v| v.is_truthy()) {
        params.insert("durationSeconds".into(), d.clone());
    }
    let token_res = client.call("GetAuthorizationToken", &Value::Map(params))?;
    let mut params = Map::new();
    params.insert("domain".into(), Value::str(&domain));
    params.insert("repository".into(), Value::str(&repository));
    params.insert("format".into(), Value::str(package_format));
    if let Some(t) = a.get("endpoint-type").filter(|v| v.is_truthy()) {
        params.insert("endpointType".into(), t.clone());
    }
    if let Some(o) = a.get("domain-owner").filter(|v| v.is_truthy()) {
        params.insert("domainOwner".into(), o.clone());
    }
    let endpoint_res = client.call("GetRepositoryEndpoint", &Value::Map(params))?;
    let endpoint = endpoint_res
        .get("repositoryEndpoint")
        .map(crate::pyfmt::py_str)
        .ok_or_else(|| general(ctx, "'repositoryEndpoint'"))?;
    let namespace = a.str("namespace");
    if !namespace_support && namespace.as_deref().is_some_and(|n| !n.is_empty()) {
        return Err(general(
            ctx,
            &format!("Argument --namespace is not supported for {tool}"),
        ));
    }
    let auth_token = token_res
        .get("authorizationToken")
        .map(crate::pyfmt::py_str)
        .ok_or_else(|| general(ctx, "'authorizationToken'"))?;
    let expiration_raw = token_res
        .get("expiration")
        .map(crate::pyfmt::py_str)
        .ok_or_else(|| general(ctx, "'expiration'"))?;
    let expiration = parse_timestamp(&expiration_raw)
        .ok_or_else(|| general(ctx, &format!("Invalid timestamp \"{expiration_raw}\"")))?;
    let login = Login {
        ctx,
        auth_token,
        expiration,
        endpoint,
        domain,
        repository,
        namespace,
    };
    let dry_run = a.bool("dry-run");
    match tool.as_str() {
        "npm" => login.npm(dry_run)?,
        "pip" => login.pip(dry_run)?,
        "twine" => login.twine(dry_run)?,
        "swift" => login.swift(dry_run)?,
        "nuget" | "dotnet" => login.nuget(&tool, dry_run)?,
        _ => unreachable!("choice"),
    }
    Ok(ExitCode::Success)
}

/// `botocore.utils.parse_timestamp` of the ISO 8601 string the CLI's
/// timestamp parser produced (or epoch seconds).
fn parse_timestamp(s: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d);
    }
    if let Ok(d) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return Some(d);
    }
    if let Ok(f) = s.parse::<f64>() {
        let secs = f.floor() as i64;
        let nanos = ((f - f.floor()) * 1e9) as u32;
        return DateTime::from_timestamp(secs, nanos).map(|d| d.fixed_offset());
    }
    None
}

/// `str(datetime)` of an aware datetime.
fn py_datetime_str(d: &DateTime<FixedOffset>) -> String {
    let mut out = d.format("%Y-%m-%d %H:%M:%S").to_string();
    let micros = d.nanosecond() / 1000;
    if micros != 0 {
        out.push_str(&format!(".{micros:06}"));
    }
    let off = d.offset().local_minus_utc();
    let sign = if off < 0 { '-' } else { '+' };
    let off = off.abs();
    out.push_str(&format!("{sign}{:02}:{:02}", off / 3600, off % 3600 / 60));
    if off % 60 != 0 {
        out.push_str(&format!(":{:02}", off % 60));
    }
    out
}

/// `relativedelta(dt1, dt2) + relativedelta(seconds=30)`: (years, months,
/// days, hours, minutes).
fn relative(dt1: DateTime<Utc>, dt2: DateTime<Utc>) -> [i64; 5] {
    let mut months =
        (dt1.year() - dt2.year()) as i64 * 12 + (dt1.month() as i64 - dt2.month() as i64);
    let add = |m: i64| -> DateTime<Utc> {
        if m >= 0 {
            dt2.checked_add_months(Months::new(m as u32)).unwrap_or(dt2)
        } else {
            dt2.checked_sub_months(Months::new((-m) as u32))
                .unwrap_or(dt2)
        }
    };
    let mut dtm = add(months);
    if dt1 < dt2 {
        while dt1 > dtm {
            months += 1;
            dtm = add(months);
        }
    } else {
        while dt1 < dtm {
            months -= 1;
            dtm = add(months);
        }
    }
    let delta = dt1 - dtm;
    let total = delta.num_seconds();
    let (days, rem) = (total.div_euclid(86400), total.rem_euclid(86400));
    let (mut days, mut hours, mut minutes, mut seconds) =
        (days, rem / 3600, rem % 3600 / 60, rem % 60 + 30);
    let (years, months) = (months / 12, months % 12);
    if seconds > 59 {
        minutes += seconds / 60;
        seconds %= 60;
    }
    let _ = seconds;
    if minutes > 59 {
        hours += minutes / 60;
        minutes %= 60;
    }
    if hours > 23 {
        days += hours / 24;
        hours %= 24;
    }
    [years, months, days, hours, minutes]
}

/// `get_relative_expiration_time`.
fn relative_expiration(values: [i64; 5]) -> String {
    let names = ["years", "months", "days", "hours", "minutes"];
    let mut out: Vec<String> = Vec::new();
    let mut prev = false;
    for (value, attr) in values.iter().zip(names) {
        if *value > 0 {
            if prev {
                out.push("and".to_string());
            }
            out.push(value.to_string());
            out.push(if *value == 1 {
                attr[..attr.len() - 1].to_string()
            } else {
                attr.to_string()
            });
        }
        if prev {
            break;
        }
        prev = *value > 0;
    }
    out.join(" ")
}

fn list_repr(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| str_repr(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

struct Login<'c> {
    ctx: &'c Context,
    auth_token: String,
    expiration: DateTime<FixedOffset>,
    endpoint: String,
    domain: String,
    repository: String,
    namespace: Option<String>,
}

enum RunError {
    NotFound,
    Failed(String),
    Os(std::io::Error),
}

/// `subprocess.run(command, capture_output=True, check=True)` under
/// `original_ld_library_path`; the captured stdout on success.
fn run_tool(command: &[String]) -> Result<Vec<u8>, RunError> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    if std::env::var_os("PYINSTALLER_RESET_ENVIRONMENT").is_none() {
        cmd.env("PYINSTALLER_RESET_ENVIRONMENT", "1");
    }
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(out.stdout),
        Ok(out) => Err(RunError::Failed(format!(
            "Command '{}' returned non-zero exit status {}.\u{0}{}",
            list_repr(command),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(RunError::NotFound),
        Err(e) => Err(RunError::Os(e)),
    }
}

impl Login<'_> {
    fn general(&self, message: &str) -> CliError {
        general(self.ctx, message)
    }

    /// `CommandFailedError`.
    fn command_failed(&self, failure: &str) -> CliError {
        let (message, stderr) = failure.split_once('\u{0}').unwrap_or((failure, ""));
        let mut msg = message.replace(&self.auth_token, "******");
        msg.push_str(&format!(" Stderr from command:\n{stderr}"));
        self.general(&msg)
    }

    fn os_error(&self, e: &std::io::Error, path: &str) -> CliError {
        let text = e.to_string();
        let text = text
            .find(" (os error")
            .map_or(text.as_str(), |i| &text[..i]);
        self.general(&format!(
            "[Errno {}] {text}: {}",
            e.raw_os_error().unwrap_or(0),
            str_repr(path)
        ))
    }

    /// `BaseLogin._run_command`.
    fn run_command(
        &self,
        tool: &str,
        command: &[String],
        ignore_errors: bool,
    ) -> Result<(), CliError> {
        match run_tool(command) {
            Ok(_) => Ok(()),
            Err(RunError::Failed(_)) if ignore_errors => Ok(()),
            Err(RunError::Failed(f)) => Err(self.command_failed(&f)),
            Err(RunError::NotFound) => Err(self.general(&format!(
                "{tool} was not found. Please verify installation."
            ))),
            Err(RunError::Os(e)) => Err(self.os_error(&e, &command[0])),
        }
    }

    /// `BaseLogin._write_success_message`.
    fn success_message(&self, tool: &str) {
        let remaining = relative(self.expiration.with_timezone(&Utc), Utc::now());
        basic::print(&format!(
            "Successfully configured {tool} to use AWS CodeArtifact repository {} \nLogin expires in {} at {}\n",
            self.endpoint,
            relative_expiration(remaining),
            py_datetime_str(&self.expiration)
        ));
    }

    /// `BaseLogin._run_commands`.
    fn run_commands(
        &self,
        tool: &str,
        commands: &[Vec<String>],
        dry_run: bool,
    ) -> Result<(), CliError> {
        if dry_run {
            for c in commands {
                basic::print(&format!("{}\n\n", c.join(" ")));
            }
            return Ok(());
        }
        for c in commands {
            let ignore = tool == "npm" && c.iter().any(|a| a.contains("always-auth"));
            self.run_command(tool, c, ignore)?;
        }
        self.success_message(tool);
        Ok(())
    }

    /// `NpmLogin.login`.
    fn npm(&self, dry_run: bool) -> Result<(), CliError> {
        let scope = match &self.namespace {
            None => None,
            Some(ns) => {
                let scope = if ns.starts_with('@') {
                    ns.clone()
                } else {
                    format!("@{ns}")
                };
                let re = regex::Regex::new(r"^(@[a-z0-9-~][a-z0-9-._~]*)").expect("regex");
                if !re.is_match(&scope) {
                    return Err(self.general(
                        "Invalid scope name, scope must contain URL-safe characters, no leading dots or underscores",
                    ));
                }
                Some(scope)
            }
        };
        let registry = match &scope {
            Some(s) if !s.is_empty() => format!("{s}:registry"),
            _ => "registry".to_string(),
        };
        let uri = urlsplit(&self.endpoint);
        let commands = vec![
            vec![
                "npm".to_string(),
                "config".to_string(),
                "set".to_string(),
                registry,
                self.endpoint.clone(),
            ],
            vec![
                "npm".to_string(),
                "config".to_string(),
                "set".to_string(),
                format!("//{}{}:always-auth", uri.netloc, uri.path),
                "true".to_string(),
            ],
        ];
        if !dry_run {
            let key = format!("//{}{}:_authToken", uri.netloc, uri.path);
            let path = match std::env::var("NPM_CONFIG_USERCONFIG") {
                Ok(c) if !c.is_empty() => expanduser(&c),
                _ => format!("{}/.npmrc", home()),
            };
            self.write_npmrc(&key, &path)?;
        }
        self.run_commands("npm", &commands, dry_run)
    }

    /// `NpmLogin._write_npmrc_value`.
    fn write_npmrc(&self, key: &str, path: &str) -> Result<(), CliError> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let new_entry = format!("{key}={}", self.auth_token);
        let p = std::path::Path::new(path);
        if !p.is_file() {
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)
                    .map_err(|e| self.os_error(&e, &dir.display().to_string()))?;
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(p)
                .map_err(|e| self.os_error(&e, path))?;
            f.write_all(format!("{new_entry}\n").as_bytes())
                .map_err(|e| self.os_error(&e, path))?;
            return Ok(());
        }
        let contents = std::fs::read_to_string(p).map_err(|e| self.os_error(&e, path))?;
        let re = regex::Regex::new(&format!(r"(?m)^{}=.*$", regex::escape(key))).expect("regex");
        let new_contents = if re.is_match(&contents) {
            re.replace_all(&contents, regex::NoExpand(&new_entry))
                .into_owned()
        } else if contents.ends_with('\n') {
            format!("{contents}{new_entry}\n")
        } else {
            format!("{contents}\n{new_entry}\n")
        };
        let dir = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let tmp = dir.join(format!(".npmrc.tmp.{:016x}", rand::random::<u64>()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| self.os_error(&e, &tmp.display().to_string()))?;
        let result = f
            .write_all(new_contents.as_bytes())
            .and_then(|_| std::fs::rename(&tmp, p));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.os_error(&e, path));
        }
        Ok(())
    }

    /// `PipLogin.login`.
    fn pip(&self, dry_run: bool) -> Result<(), CliError> {
        let uri = urlsplit(&self.endpoint);
        let url = format!(
            "{}://aws:{}@{}{}simple/",
            uri.scheme, self.auth_token, uri.netloc, uri.path
        );
        let commands = vec![vec![
            "pip".to_string(),
            "config".to_string(),
            "set".to_string(),
            "global.index-url".to_string(),
            url,
        ]];
        self.run_commands("pip", &commands, dry_run)
    }

    /// `TwineLogin.login`.
    fn twine(&self, dry_run: bool) -> Result<(), CliError> {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let path = format!("{}/.pypirc", home());
        let text = if std::path::Path::new(&path).exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            match update_pypirc(&content, &self.endpoint, &self.auth_token) {
                Ok(t) => t,
                Err(e) => {
                    basic::print(&format!("{path} is in an invalid state.\n"));
                    return Err(self.general(&e));
                }
            }
        } else {
            let mut ini = Ini::default();
            ini.set("distutils", "index-servers", "\npypi\ncodeartifact");
            ini.set("codeartifact", "repository", &self.endpoint);
            ini.set("codeartifact", "username", "aws");
            ini.set("codeartifact", "password", &self.auth_token);
            ini.write()
        };
        if dry_run {
            basic::print(&format!(
                "Dryrun mode is enabled, not writing to pypirc.\n{path} would have been set to the following:\n\n{text}\n"
            ));
            return Ok(());
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| self.os_error(&e, &path))?;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            basic::eprint(&format!("Unable to set file permissions for {path}: {e}\n"));
        }
        f.write_all(text.as_bytes())
            .map_err(|e| self.os_error(&e, &path))?;
        self.success_message("twine");
        Ok(())
    }

    /// `SwiftLogin.login` (not macOS: the token goes to `~/.netrc`).
    fn swift(&self, dry_run: bool) -> Result<(), CliError> {
        if let Some(ns) = &self.namespace
            && !swift_scope_valid(ns)
        {
            return Err(self.general(
                "Invalid scope name, scope must contain URL-safe characters, no leading dots or underscores and no more than 39 characters",
            ));
        }
        let mut set = vec![
            "swift".to_string(),
            "package-registry".to_string(),
            "set".to_string(),
            self.endpoint.clone(),
        ];
        if let Some(ns) = &self.namespace {
            set.push("--scope".to_string());
            set.push(ns.clone());
        }
        let commands = vec![
            set,
            vec![
                "swift".to_string(),
                "package-registry".to_string(),
                "login".to_string(),
                format!("{}login", self.endpoint),
            ],
        ];
        let hostname = crate::session::url::UrlParts::parse(&self.endpoint)
            .map(|p| p.hostname)
            .unwrap_or_default();
        let entry = format!(
            "machine {hostname} login token password {}",
            self.auth_token
        );
        let netrc = format!("{}/.netrc", home());
        if dry_run {
            basic::print(&format!(
                "Dryrun mode is enabled, not writing to netrc.\nThe following line would have been written to {netrc}:\n\n{entry}\n\nAnd would have run the following commands:\n\n"
            ));
        } else {
            self.update_netrc(&hostname, &entry, &netrc)?;
        }
        self.run_commands("swift", &commands, dry_run)
    }

    /// `SwiftLogin._update_netrc_entry`.
    fn update_netrc(&self, hostname: &str, entry: &str, path: &str) -> Result<(), CliError> {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let p = std::path::Path::new(path);
        if !p.is_file() {
            if let Some(dir) = p.parent()
                && !dir.is_dir()
            {
                std::fs::create_dir_all(dir).map_err(|e| self.os_error(&e, path))?;
            }
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(p)
                .map_err(|e| self.os_error(&e, path))?;
            return f
                .write_all(format!("{entry}\n").as_bytes())
                .map_err(|e| self.os_error(&e, path));
        }
        let contents = std::fs::read_to_string(p).map_err(|e| self.os_error(&e, path))?;
        let re = regex::Regex::new(&format!(
            r"(?m)(?P<entry_start>\bmachine\s+{}\s+login\s+\S+\s+password\s+)(?P<token>\S+)",
            regex::escape(hostname)
        ))
        .expect("regex");
        let token = self.auth_token.clone();
        let mut new_contents = re
            .replace_all(&contents, |c: &regex::Captures<'_>| {
                format!("{}{token}", &c["entry_start"])
            })
            .into_owned();
        if new_contents == contents {
            new_contents = if contents.ends_with('\n') {
                format!("{contents}{entry}\n")
            } else {
                format!("{contents}\n{entry}\n")
            };
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)
            .map_err(|e| self.os_error(&e, path))?;
        if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)) {
            basic::eprint(&format!("Unable to set file permissions for {path}: {e}\n"));
        }
        f.write_all(new_contents.as_bytes())
            .map_err(|e| self.os_error(&e, path))
    }

    /// `NuGetBaseLogin.login` (`NuGetLogin` / `DotNetLogin`).
    fn nuget(&self, tool: &str, dry_run: bool) -> Result<(), CliError> {
        let list: Vec<String> = if tool == "nuget" {
            &["nuget", "sources", "list", "-format", "detailed"][..]
        } else {
            &["dotnet", "nuget", "list", "source", "--format", "detailed"][..]
        }
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut cmd = Command::new(&list[0]);
        cmd.args(&list[1..]);
        if std::env::var_os("PYINSTALLER_RESET_ENVIRONMENT").is_none() {
            cmd.env("PYINSTALLER_RESET_ENVIRONMENT", "1");
        }
        let output = match cmd.stderr(std::process::Stdio::piped()).output() {
            Ok(o) if o.status.success() => o.stdout,
            Ok(o) => {
                return Err(self.general(&format!(
                    "Command '{}' returned non-zero exit status {}.",
                    list_repr(&list),
                    o.status.code().unwrap_or(-1)
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(self.general(&format!(
                    "{tool} was not found. Please verify installation."
                )));
            }
            Err(e) => return Err(self.os_error(&e, &list[0])),
        };
        let sources = parse_sources(&String::from_utf8_lossy(&output));
        let index_url = format!("{}v3/index.json", self.endpoint);
        let default_name = format!("{}/{}", self.domain, self.repository);
        let existing = sources
            .iter()
            .find(|(_, url)| *url == index_url)
            .or_else(|| sources.iter().find(|(name, _)| *name == default_name))
            .map(|(name, _)| name.clone());
        let (operation, source_name) = match &existing {
            Some(n) => ("update", n.clone()),
            None => ("add", default_name),
        };
        let token = self.auth_token.clone();
        let command: Vec<String> = if tool == "nuget" {
            vec![
                "nuget".into(),
                "sources".into(),
                operation.into(),
                "-name".into(),
                source_name.clone(),
                "-source".into(),
                index_url,
                "-username".into(),
                "aws".into(),
                "-password".into(),
                token,
            ]
        } else {
            let mut c: Vec<String> = vec![
                "dotnet".into(),
                "nuget".into(),
                operation.into(),
                "source".into(),
            ];
            if operation == "add" {
                c.push(index_url);
                c.extend(["--name".into(), source_name.clone()]);
            } else {
                c.push(source_name.clone());
                c.extend(["--source".into(), index_url]);
            }
            c.extend([
                "--username".into(),
                "aws".into(),
                "--password".into(),
                token,
                "--store-password-in-clear-text".into(),
            ]);
            c
        };
        if dry_run {
            basic::print(&format!("{}\n", command.join(" ")));
            return Ok(());
        }
        match run_tool(&command) {
            Ok(_) => {}
            Err(RunError::Failed(f)) => {
                basic::print("Failed to update the NuGet.Config\n");
                return Err(self.command_failed(&f));
            }
            Err(RunError::NotFound) => {
                return Err(self.general(&format!(
                    "[Errno 2] No such file or directory: {}",
                    str_repr(tool)
                )));
            }
            Err(RunError::Os(e)) => return Err(self.os_error(&e, tool)),
        }
        if existing.is_some() {
            basic::print(&format!(
                "Updated source {source_name} in the NuGet.Config\n"
            ));
        } else {
            basic::print(&format!(
                "Added source {source_name} to the user level NuGet.Config\n"
            ));
        }
        self.success_message("nuget");
        Ok(())
    }
}

/// The `\A...\Z` swift scope pattern (`(?=...)` lookahead unrolled).
fn swift_scope_valid(ns: &str) -> bool {
    let chars: Vec<char> = ns.chars().collect();
    if chars.is_empty() || chars.len() > 39 || !chars[0].is_ascii_alphanumeric() {
        return false;
    }
    for (i, c) in chars.iter().enumerate().skip(1) {
        if c.is_ascii_alphanumeric() {
            continue;
        }
        if *c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_alphanumeric) {
            continue;
        }
        return false;
    }
    true
}

/// `NuGetBaseLogin._get_source_to_url_dict`.
fn parse_sources(output: &str) -> Vec<(String, String)> {
    let re = regex::Regex::new(r"^\d+\.\s(?P<name>.+)\s\[.*\]").expect("regex");
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = re.captures(line.trim()) {
            let name = c["name"].trim().to_string();
            let url = lines
                .get(i + 1)
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some(e) => e.1 = url,
                None => out.push((name, url)),
            }
        }
    }
    out
}

/// The parts of `configparser.RawConfigParser` `.pypirc` handling uses.
#[derive(Default)]
struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    fn parse(text: &str) -> Result<Ini, String> {
        let mut ini = Ini::default();
        let mut current: Option<usize> = None;
        let mut last_key: Option<usize> = None;
        let mut blank_pending = 0usize;
        for (lineno, raw) in text.lines().enumerate() {
            let stripped = raw.trim();
            if stripped.starts_with('#') || stripped.starts_with(';') {
                continue;
            }
            if stripped.is_empty() {
                blank_pending += 1;
                continue;
            }
            let indented = raw.starts_with(' ') || raw.starts_with('\t');
            if indented && let (Some(s), Some(k)) = (current, last_key) {
                let v = &mut ini.sections[s].1[k].1;
                for _ in 0..blank_pending {
                    v.push('\n');
                }
                v.push('\n');
                v.push_str(stripped);
                blank_pending = 0;
                continue;
            }
            blank_pending = 0;
            if stripped.starts_with('[') && stripped.ends_with(']') {
                let name = stripped[1..stripped.len() - 1].to_string();
                current = Some(match ini.sections.iter().position(|(n, _)| *n == name) {
                    Some(i) => i,
                    None => {
                        ini.sections.push((name, Vec::new()));
                        ini.sections.len() - 1
                    }
                });
                last_key = None;
                continue;
            }
            let Some(s) = current else {
                return Err(format!(
                    "File contains no section headers.\nfile: '<???>', line: {}\n{}",
                    lineno + 1,
                    str_repr(&format!("{raw}\n"))
                ));
            };
            let pos = raw.find(['=', ':']);
            let (k, v) = match pos {
                Some(p) => (raw[..p].trim(), raw[p + 1..].trim()),
                None => (stripped, ""),
            };
            let key = k.to_lowercase();
            let entries = &mut ini.sections[s].1;
            match entries.iter().position(|(ek, _)| *ek == key) {
                Some(i) => {
                    entries[i].1 = v.to_string();
                    last_key = Some(i);
                }
                None => {
                    entries.push((key, v.to_string()));
                    last_key = Some(entries.len() - 1);
                }
            }
        }
        Ok(ini)
    }

    fn get(&self, section: &str, key: &str) -> Result<String, String> {
        let (_, entries) = self
            .sections
            .iter()
            .find(|(n, _)| n == section)
            .ok_or_else(|| format!("No section: {}", str_repr(section)))?;
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                format!(
                    "No option {} in section: {}",
                    str_repr(key),
                    str_repr(section)
                )
            })
    }

    fn set(&mut self, section: &str, key: &str, value: &str) {
        let s = match self.sections.iter().position(|(n, _)| n == section) {
            Some(i) => i,
            None => {
                self.sections.push((section.to_string(), Vec::new()));
                self.sections.len() - 1
            }
        };
        let entries = &mut self.sections[s].1;
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_string(),
            None => entries.push((key.to_string(), value.to_string())),
        }
    }

    fn write(&self) -> String {
        let mut out = String::new();
        for (name, entries) in &self.sections {
            out.push_str(&format!("[{name}]\n"));
            for (k, v) in entries {
                out.push_str(&format!("{k} = {}\n", v.replace('\n', "\n\t")));
            }
            out.push('\n');
        }
        out
    }
}

/// `TwineLogin.get_commands` for an existing `.pypirc`.
fn update_pypirc(content: &str, endpoint: &str, token: &str) -> Result<String, String> {
    let mut ini = Ini::parse(content)?;
    let servers_raw = ini.get("distutils", "index-servers")?;
    let mut servers: Vec<String> = servers_raw
        .split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if !servers.iter().any(|s| s == "codeartifact") {
        servers.push("codeartifact".to_string());
        ini.set(
            "distutils",
            "index-servers",
            &format!("\n{}", servers.join("\n")),
        );
    }
    ini.set("codeartifact", "repository", endpoint);
    ini.set("codeartifact", "username", "aws");
    ini.set("codeartifact", "password", token);
    Ok(ini.write())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn expiration_text() {
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 1, 2, 15, 4, 5).unwrap();
        assert_eq!(relative_expiration(relative(later, now)), "12 hours");
        let later = Utc.with_ymd_and_hms(2026, 1, 2, 4, 5, 40).unwrap();
        assert_eq!(
            relative_expiration(relative(later, now)),
            "1 hour and 2 minutes"
        );
        let later = Utc.with_ymd_and_hms(2026, 3, 5, 3, 4, 5).unwrap();
        assert_eq!(
            relative_expiration(relative(later, now)),
            "2 months and 3 days"
        );
        let d = parse_timestamp("2026-11-04T05:00:00+00:00").unwrap();
        assert_eq!(py_datetime_str(&d), "2026-11-04 05:00:00+00:00");
    }

    #[test]
    fn pypirc() {
        let out = update_pypirc(
            "[distutils]\nindex-servers =\n  pypi\n\n[pypi]\nusername: me\n",
            "https://e/",
            "tok",
        )
        .unwrap();
        assert_eq!(
            out,
            "[distutils]\nindex-servers = \n\tpypi\n\tcodeartifact\n\n[pypi]\nusername = me\n\n[codeartifact]\nrepository = https://e/\nusername = aws\npassword = tok\n\n"
        );
        assert_eq!(
            update_pypirc("[a]\n", "e", "t").unwrap_err(),
            "No section: 'distutils'"
        );
    }

    #[test]
    fn sources() {
        let s = parse_sources(
            "Registered Sources:\n\n  1.  nuget.org [Enabled]\n      https://api.nuget.org/v3/index.json\n  2.  d/r [Disabled]\n      https://x/v3/index.json\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].0, "d/r");
    }
}
