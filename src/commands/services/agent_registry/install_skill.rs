//! `awscli/customizations/agentregistry/install_skill.py`: download,
//! verify and install a skill from a registry, from its package or from
//! the SKILL.md text the record holds.

use std::collections::HashSet;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization as _;

use super::basic::{self, ArgDef, Args};
use super::constants::HTTP_TIMEOUT;
use super::dataplane::{create_data_plane_client, sign_in_arguments, skill_record_pages};
use super::frontmatter::{MAX_SKILL_MD_BYTES, frontmatter_name};
use super::urlreq::{self, Request, UrlError};
use super::utils::{
    bare_registry_id, comparable_endpoint, printable, py_strip, require_https_jwt, str_of,
    url_origin, write_status,
};
use super::zipread::{OpenError, ZipFile, ZipInfo};
use super::{Error, Result};
use crate::commands::services::agent_toolkit::{
    SKILL_METADATA_FILENAME, agent_configs, realpath, resolve_agents, universal_first,
};
use crate::commands::services::exec::Client;
use crate::driver::Context;
use crate::error::ExitCode;
use crate::value::{Map, Value};

const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
pub(crate) const MAX_SCANNED_ENTRIES: usize = 10_000;
const CHUNK_BYTES: usize = 64 * 1024;

static SKILL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[a-z0-9](?:[a-z0-9]*(?:-[a-z0-9]+)*)?\z").expect("regex"));
const MAX_SKILL_NAME_LENGTH: usize = 64;
pub(crate) const SKILL_NAME_RULE: &str = "at most 64 lowercase letters, digits and hyphens, with no hyphen at the start or end and no two hyphens in a row";
static LEGACY_FOLDER_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[A-Za-z0-9][A-Za-z0-9._-]*\z").expect("regex"));
pub(crate) const FOLDER_NAME_RULE: &str =
    "at most 64 letters, digits, dots, underscores and hyphens, beginning with a letter or digit";
static UNPORTABLE_CHARACTERS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[\x00-\x1f<>:"|?*]"#).expect("regex"));
static SHA256_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[0-9a-f]{64}\z").expect("regex"));

pub(crate) const SKILL_MARKER: &str = "SKILL.md";
const SKILL_MD_NAMES: [&str; 2] = ["SKILL.md", "skill.md"];
pub(crate) const PROVENANCE_MARKER: &str = ".agent-registry-skill.json";
const CONTENT_PACKAGE: &str = "package";
const CONTENT_SKILL_MD: &str = "skillMd";
const MAX_SKILL_MD_CHARACTERS: usize = 102_400;
const MAX_PROVENANCE_BYTES: u64 = 64 * 1024;

/// `_RESERVED_NAMES`.
fn reserved_names() -> [(&'static str, &'static str); 2] {
    [
        (
            PROVENANCE_MARKER,
            "the file in which install-skill notes where a skill came from",
        ),
        (SKILL_METADATA_FILENAME, "agent-toolkit's install metadata"),
    ]
}

/// `_CONTENT_LABELS`.
fn content_label_of(content: &str) -> Option<&'static str> {
    match content {
        CONTENT_PACKAGE => Some("package"),
        CONTENT_SKILL_MD => Some("SKILL.md"),
        _ => None,
    }
}

/// `AGENT_ARGUMENT`.
pub(crate) fn agent_argument() -> ArgDef {
    ArgDef::new("agent").type_name("string")
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `is_spec_skill_name(name)`.
pub(crate) fn is_spec_skill_name(name: &str) -> bool {
    char_len(name) <= MAX_SKILL_NAME_LENGTH && SKILL_NAME.is_match(name)
}

/// `folder_name_problem(name)`.
pub(crate) fn folder_name_problem(name: &str) -> Option<String> {
    if is_spec_skill_name(name)
        || (char_len(name) <= MAX_SKILL_NAME_LENGTH && LEGACY_FOLDER_NAME.is_match(name))
    {
        return None;
    }
    Some(format!(
        "a skill's folder name must be a skill name under the Agent Skills specification ({SKILL_NAME_RULE}) or, for an older skill, {FOLDER_NAME_RULE}"
    ))
}

/// `safe_folder_name(name)`.
pub(crate) fn safe_folder_name(name: &str) -> Result<String> {
    match folder_name_problem(name) {
        Some(problem) => Err(Error::Jwt(format!(
            "Refusing a skill named {}: {problem}.",
            printable(&crate::pyfmt::str_repr(name))
        ))),
        None => Ok(name.to_string()),
    }
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// `os.path.dirname`.
pub(crate) fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => {
            let head = &path[..i + 1];
            let trimmed = head.trim_end_matches('/');
            if trimmed.is_empty() {
                head.to_string()
            } else {
                trimmed.to_string()
            }
        }
        None => String::new(),
    }
}

/// `os.path.basename`.
pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `skill_path(target_root, name)`.
pub(crate) fn skill_path(target_root: &str, name: &str) -> Result<String> {
    let root = realpath(target_root);
    let path = join(&root, &safe_folder_name(name)?);
    if dirname(&realpath(&path)) != root {
        return Err(Error::Jwt(format!(
            "Refusing {path}: it resolves outside {root}."
        )));
    }
    Ok(path)
}

pub(crate) fn islink(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

pub(crate) fn lexists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `is_installed_skill(path)`.
pub(crate) fn is_installed_skill(path: &str) -> bool {
    !islink(path)
        && std::path::Path::new(path).is_dir()
        && std::path::Path::new(&join(path, SKILL_MARKER)).is_file()
}

/// A provenance marker (`read_provenance`): the four fields and
/// `content`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Marker {
    pub registry_id: String,
    pub record_id: String,
    pub endpoint: String,
    pub sha256: String,
    pub content: String,
}

/// `read_provenance(skill_dir)`.
pub(crate) fn read_provenance(skill_dir: &str) -> Option<Marker> {
    let path = join(skill_dir, PROVENANCE_MARKER);
    let info = std::fs::symlink_metadata(&path).ok()?;
    if !info.file_type().is_file() || info.len() > MAX_PROVENANCE_BYTES {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .ok()?;
    if !file.metadata().ok()?.file_type().is_file() {
        return None;
    }
    let mut data = Vec::new();
    use std::io::Read as _;
    (&file)
        .take(MAX_PROVENANCE_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_PROVENANCE_BYTES {
        return None;
    }
    let text = super::utils::decode_utf8(&data).ok()?;
    let marker = crate::cli::pyjson::loads(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()?;
    if !matches!(marker, Value::Map(_)) {
        return None;
    }
    let field = |k: &str| str_of(&marker, k).map(str::to_string);
    let content = match marker.get("content") {
        None => CONTENT_PACKAGE.to_string(),
        Some(Value::Str(s)) => s.clone(),
        Some(_) => return None,
    };
    Some(Marker {
        registry_id: field("registryId")?,
        record_id: field("recordId")?,
        endpoint: field("endpoint")?,
        sha256: field("sha256")?,
        content,
    })
}

/// `source_key(source)`: `None` where it raises `ValueError`.
pub(crate) fn source_key(
    registry_id: &str,
    record_id: &str,
    endpoint: &str,
) -> Option<(String, String, (String, String, String))> {
    crate::endpoint::pyurl::urlsplit_parts(endpoint).ok()?;
    Some((
        registry_id.to_string(),
        record_id.to_string(),
        comparable_endpoint(endpoint),
    ))
}

impl Marker {
    fn key(&self) -> Option<(String, String, (String, String, String))> {
        source_key(&self.registry_id, &self.record_id, &self.endpoint)
    }
}

/// `same_source(installed, source)`.
fn same_source(installed: &Marker, source: &Marker) -> bool {
    installed.key() == source.key()
}

/// `is_known_content(installed)`.
fn is_known_content(installed: &Marker) -> bool {
    content_label_of(&installed.content).is_some()
}

/// `keeps_package(installed, source)`.
fn keeps_package(installed: &Marker, source: &Marker) -> bool {
    installed.content == CONTENT_PACKAGE && source.content == CONTENT_SKILL_MD
}

/// `same_content(installed, source)`.
fn same_content(installed: &Marker, source: &Marker) -> bool {
    installed.content == source.content && installed.sha256 == source.sha256
}

/// `installed_skills(folder)`: `(name, path, marker)`, sorted by name.
pub(crate) fn installed_skills(folder: &str) -> Vec<(String, String, Marker)> {
    let root = realpath(folder);
    let mut found = Vec::new();
    match std::fs::read_dir(&root) {
        Ok(entries) => {
            for (count, entry) in entries.enumerate() {
                if count >= MAX_SCANNED_ENTRIES {
                    crate::debuglog::debug(
                        "awscli.customizations.agentregistry.install_skill",
                        &format!(
                            "Looked at the first {MAX_SCANNED_ENTRIES} entries of {root} only."
                        ),
                    );
                    break;
                }
                let Ok(entry) = entry else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = join(&root, &name);
                if name.starts_with('.')
                    || islink(&path)
                    || folder_name_problem(&name).is_some()
                    || !is_installed_skill(&path)
                {
                    continue;
                }
                let Some(marker) = read_provenance(&path) else {
                    continue;
                };
                if marker.key().is_none() {
                    continue;
                }
                found.push((name, path, marker));
            }
        }
        Err(e) => crate::debuglog::debug(
            "awscli.customizations.agentregistry.install_skill",
            &format!(
                "Could not read the skills folder {root}: {}",
                os_error_repr(&e)
            ),
        ),
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// `repr(OSError)`, as `%r` logs it.
fn os_error_repr(e: &std::io::Error) -> String {
    let name = match e.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        std::io::ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
    };
    match e.raw_os_error() {
        Some(c) => {
            let text = std::io::Error::from_raw_os_error(c).to_string();
            let text = text.split(" (os error").next().unwrap_or_default();
            format!("{name}({c}, {})", crate::pyfmt::str_repr(text))
        }
        None => format!("{name}({})", crate::pyfmt::str_repr(&e.to_string())),
    }
}

/// `installed_copies(folder, source)`.
fn installed_copies(folder: &str, source: &Marker) -> Vec<(String, String, Marker)> {
    installed_skills(folder)
        .into_iter()
        .filter(|(_, _, m)| same_source(m, source))
        .collect()
}

/// `source_detail(marker, with_endpoint)`.
fn source_detail(marker: &Marker, with_endpoint: bool) -> String {
    let mut detail = format!(
        "registry {}, record {}",
        printable(&marker.registry_id),
        printable(&marker.record_id)
    );
    if with_endpoint {
        detail.push_str(&format!(" at {}", printable(&marker.endpoint)));
    }
    detail
}

/// `describe_source(marker)`.
pub(crate) fn describe_source(marker: &Marker) -> String {
    format!("from {}", source_detail(marker, false))
}

/// A skills folder: `(agent name, folder, agent id)`.
pub(crate) type Folder = (Option<String>, String, Option<String>);

/// `resolve_skill_folders(parsed_args, agent_configs)`.
pub(crate) fn resolve_skill_folders(ctx: &Context, args: &Args) -> Result<Vec<Folder>> {
    let agent_filter = args.str("agent").filter(|a| !a.is_empty());
    let target_directory = args.str("target-directory").filter(|t| !t.is_empty());
    if agent_filter.is_some() && target_directory.is_some() {
        return Err(Error::Param(
            "--agent and --target-directory cannot be used together.".to_string(),
        ));
    }
    if let Some(t) = target_directory {
        return Ok(vec![(None, crate::cli::paramfile::expanduser(&t), None)]);
    }
    let configs = agent_configs();
    let mut agents = resolve_agents(ctx, &configs, agent_filter.as_deref()).map_err(Error::Cli)?;
    universal_first(&mut agents, |a| a.config.id);
    let mut seen = HashSet::new();
    let mut folders = Vec::new();
    for agent in &agents {
        let skills = agent.skills_path();
        if !seen.insert(realpath(&skills)) {
            continue;
        }
        folders.push((
            Some(agent.config.display_name.clone()),
            skills,
            Some(agent.config.id.to_string()),
        ));
    }
    Ok(folders)
}

/// `report(agent, message)`.
pub(crate) fn report(agent: Option<&str>, message: &str) {
    match agent {
        Some(a) => write_status(&format!("{a}: {message}")),
        None => write_status(message),
    }
}

/// `is_s3_host(host, region)`.
fn is_s3_host(host: &str, region: &str) -> bool {
    let host = host.to_lowercase();
    let r = regex::escape(region);
    let bucket = r"(?:[a-z0-9][a-z0-9.-]*\.)?";
    let mut patterns = vec![
        format!(
            r"^{bucket}(?:s3|s3-fips|s3-accesspoint|s3-accesspoint-fips)(?:\.dualstack)?\.{r}\.amazonaws\.com(?:\.cn)?$"
        ),
        format!(r"^{bucket}s3-{r}\.amazonaws\.com$"),
    ];
    if region == "us-east-1" {
        patterns.push(format!(r"^{bucket}s3\.amazonaws\.com$"));
    }
    patterns
        .iter()
        .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(&host)))
}

/// `validate_download_url(url, region)`.
fn validate_download_url(url: &str, region: &str) -> Result<String> {
    require_https_jwt(url, "download URL")?;
    let host = crate::endpoint::pyurl::urlsplit_parts(url)
        .ok()
        .and_then(|p| p.hostname())
        .unwrap_or_default();
    if !is_s3_host(&host, region) {
        return Err(Error::Jwt(format!(
            "Refusing to download from {}: the download URL must point at Amazon S3 in {region}.",
            url_origin(url)
        )));
    }
    Ok(url.to_string())
}

/// `InstallSkillCommand.ARG_TABLE`.
fn arg_table() -> Vec<ArgDef> {
    let mut defs = vec![
        ArgDef::new("registry-id").required(),
        ArgDef::new("record-id"),
        ArgDef::new("record-name"),
        agent_argument(),
        ArgDef::new("target-directory"),
    ];
    defs.extend(sign_in_arguments());
    defs.push(ArgDef::new("force").flag());
    defs
}

/// The kind of content a record publishes, with its payload.
enum Payload {
    Package(String),
    SkillMd(Vec<u8>),
}

/// `InstallSkillCommand._run_main`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode> {
    let defs = arg_table();
    let Some(a) =
        basic::parse(ctx, &["agent-registry", "install-skill"], &defs, args).map_err(Error::Cli)?
    else {
        return Ok(ExitCode::Success);
    };
    let wanted_name = a.str("record-name").filter(|n| !n.is_empty());
    let record_id_arg = a.str("record-id").filter(|n| !n.is_empty());
    if record_id_arg.is_some() == wanted_name.is_some() {
        return Err(Error::Param(
            "Specify exactly one of --record-id and --record-name.".to_string(),
        ));
    }
    let folders = resolve_skill_folders(ctx, &a)?;
    if folders.is_empty() {
        return Err(Error::Param(
            "No supported AI coding agents found. Pass --target-directory <dir> to install into a folder of your choice, or --agent <id> once that agent is set up on this machine.".to_string(),
        ));
    }
    let data_plane = create_data_plane_client(ctx, &a)?;
    let client = data_plane.client(ctx)?;
    let registry_arg = a.str("registry-id").unwrap_or_default();

    let requested = match &wanted_name {
        Some(name) => {
            let id = resolve_record_name(&client, &registry_arg, name)?;
            write_status(&format!(
                "Record {} is {}.",
                printable(&crate::pyfmt::str_repr(name)),
                printable(&id)
            ));
            id
        }
        None => record_id_arg.unwrap_or_default(),
    };
    let record = get_record(&client, &registry_arg, &requested)?;
    let record_id = record_id_of(&record, &requested);
    let record_name = record.get("name").cloned();
    let shown = record_display_name(&record, &record_id);
    let (payload, digest) = read_content(&record, &requested)?;
    let source = Marker {
        registry_id: bare_registry_id(&registry_arg)?,
        record_id: record_id.clone(),
        endpoint: data_plane.endpoint.clone(),
        sha256: digest.clone(),
        content: match payload {
            Payload::Package(_) => CONTENT_PACKAGE,
            Payload::SkillMd(_) => CONTENT_SKILL_MD,
        }
        .to_string(),
    };
    let force = a.bool("force");

    let mut pending = Vec::new();
    let mut failed = false;
    for (agent, folder, agent_id) in &folders {
        let copies = installed_copies(folder, &source);
        if let Some((_, path, _)) = copies.iter().find(|(_, _, m)| !is_known_content(m)) {
            report(agent.as_deref(), &newer_install(path));
            failed = true;
            continue;
        }
        if !force {
            let left: Vec<(&str, &String, &String, &Marker)> = copies
                .iter()
                .filter(|(_, _, m)| same_content(m, &source))
                .map(|(n, p, m)| (UP_TO_DATE, n, p, m))
                .chain(
                    copies
                        .iter()
                        .filter(|(_, _, m)| keeps_package(m, &source))
                        .map(|(n, p, m)| (KEPT, n, p, m)),
                )
                .collect();
            if let Some((action, name, path, marker)) = left.first() {
                report(
                    agent.as_deref(),
                    &outcome(action, name, path, Some(marker), &source),
                );
                report_earlier_copies(agent.as_deref(), &copies, path, folder, agent_id.as_deref());
                continue;
            }
        }
        pending.push((agent.clone(), folder.clone(), agent_id.clone(), copies));
    }
    if pending.is_empty() {
        return Ok(if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::Success
        });
    }

    match payload {
        Payload::SkillMd(data) => {
            let name = named(frontmatter_name(&data, "the record's SKILL.md"), &shown)?;
            write_status(&format!(
                "Installing the SKILL.md text published on record {}; the record has no package, so only SKILL.md is installed.",
                printable(&record_id)
            ));
            note_record_name(record_name.as_ref(), &name);
            Ok(install_all(
                &pending,
                &name,
                &|staged| write_skill_md(staged, &data),
                &source,
                force,
                failed,
            ))
        }
        Payload::Package(url) => {
            let url = validate_download_url(&url, &data_plane.region)?;
            let scratch = TempDir::new()?;
            let archive = join(&scratch.0, "package.zip");
            write_status(&format!("Downloading {shown} from {}...", url_origin(&url)));
            download(&url, &archive, data_plane.ca_bundle.as_deref())?;
            let actual = sha256_file(&archive).map_err(Error::Other)?;
            if !constant_time_eq(actual.as_bytes(), digest.as_bytes()) {
                return Err(Error::Jwt(format!(
                    "Refusing to install {shown}: the archive does not match the SHA-256 digest published on the record.\n  expected {digest}\n  actual   {actual}"
                )));
            }
            write_status(&format!("Verified SHA-256 {digest}"));
            let name = check_archive(&archive, &shown)?;
            note_record_name(record_name.as_ref(), &name);
            Ok(install_all(
                &pending,
                &name,
                &|staged| safe_extract(&archive, staged),
                &source,
                force,
                failed,
            ))
        }
    }
}

/// `hmac.compare_digest`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `tempfile.TemporaryDirectory(prefix='agent-registry-')`.
struct TempDir(String);

impl TempDir {
    fn new() -> Result<TempDir> {
        let base = std::env::temp_dir().to_string_lossy().into_owned();
        Ok(TempDir(
            mkdtemp(&base, "agent-registry-").map_err(Error::Other)?,
        ))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `tempfile.mkdtemp(prefix=prefix, dir=dir)`: `Err` is `str(OSError)`.
fn mkdtemp(dir: &str, prefix: &str) -> std::result::Result<String, String> {
    use std::os::unix::fs::DirBuilderExt as _;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    loop {
        let name: String = (0..8)
            .map(|_| CHARS[rand::random_range(0..CHARS.len())] as char)
            .collect();
        let path = join(dir, &format!("{prefix}{name}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(crate::cli::paramfile::os_error(&e, &path)),
        }
    }
}

type Populate<'a> = &'a dyn Fn(&str) -> Result<()>;

/// A folder to install into: the agent, the folder, the agent id, and
/// the copies installed there from the same source.
type Pending = (
    Option<String>,
    String,
    Option<String>,
    Vec<(String, String, Marker)>,
);

/// `_install_all(pending, name, populate, source, force, failed)`.
fn install_all(
    pending: &[Pending],
    name: &str,
    populate: Populate<'_>,
    source: &Marker,
    force: bool,
    mut failed: bool,
) -> ExitCode {
    for (agent, folder, agent_id, copies) in pending {
        if !install_into(
            populate,
            name,
            agent.as_deref(),
            folder,
            agent_id.as_deref(),
            copies,
            source,
            force,
        ) {
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::Success
    }
}

/// What a planning step refuses (`_Refused`) or fails with.
enum PlanError {
    Refused(String),
    Failed(String),
}

/// `_install_into(...)`.
#[allow(clippy::too_many_arguments)]
fn install_into(
    populate: Populate<'_>,
    name: &str,
    agent: Option<&str>,
    folder: &str,
    agent_id: Option<&str>,
    copies: &[(String, String, Marker)],
    source: &Marker,
    force: bool,
) -> bool {
    let uninstall = uninstall_command(name, folder, agent_id);
    let planned = skill_path(folder, name)
        .map_err(|e| e.message())
        .and_then(|d| {
            plan_destination(&d, force, source, &uninstall)
                .map(|p| (d, p))
                .map_err(|e| match e {
                    PlanError::Refused(m) | PlanError::Failed(m) => m,
                })
        });
    let (destination, (mut action, mut installed)) = match planned {
        Ok(p) => p,
        Err(m) => {
            report(agent, &m);
            return false;
        }
    };
    if !LEFT_AS_IT_IS.contains(&action) {
        match install(populate, name, &destination, force, source, &uninstall) {
            Ok((a, i)) => {
                action = a;
                installed = i;
            }
            Err(PlanError::Refused(m)) => {
                report(agent, &m);
                return false;
            }
            Err(PlanError::Failed(m)) => {
                report(
                    agent,
                    &format!(
                        "Could not install {name} to {destination}: {}",
                        printable(&m)
                    ),
                );
                return false;
            }
        }
    }
    report(
        agent,
        &outcome(action, name, &destination, installed.as_ref(), source),
    );
    report_earlier_copies(agent, copies, &destination, folder, agent_id);
    true
}

/// `_get_record(client, registry_id, record_id)`.
fn get_record(client: &Client<'_>, registry_id: &str, record_id: &str) -> Result<Value> {
    let mut entry = Map::new();
    entry.insert("registryId".to_string(), Value::str(registry_id));
    entry.insert(
        "recordIds".to_string(),
        Value::List(vec![Value::str(record_id)]),
    );
    let mut params = Map::new();
    params.insert("entries".to_string(), Value::List(vec![Value::Map(entry)]));
    let response = client
        .call("BatchGetDiscoverableRegistryRecord", &Value::Map(params))
        .map_err(Error::Cli)?;
    let records = match response.get("registryRecords") {
        Some(Value::List(l)) => l.clone(),
        _ => vec![],
    };
    for record in &records {
        if matches!(record, Value::Map(_)) && is_record(record, record_id) {
            return Ok(record.clone());
        }
    }
    let errors: Vec<&Value> = match response.get("errors") {
        Some(Value::List(l)) => l.iter().filter(|e| matches!(e, Value::Map(_))).collect(),
        _ => vec![],
    };
    if !errors.is_empty() {
        let details: Vec<String> = errors
            .iter()
            .map(|e| {
                let code = printable(&py_str_of(e.get("errorCode")));
                format!(
                    "{}: {}",
                    if code.is_empty() {
                        "error".to_string()
                    } else {
                        code
                    },
                    printable(&py_str_of(e.get("message")))
                )
            })
            .collect();
        return Err(Error::Jwt(format!(
            "Could not get record {record_id} from registry {registry_id}: {}",
            details.join("; ")
        )));
    }
    if !records.is_empty() {
        return Err(Error::Jwt(format!(
            "The registry returned a record other than {record_id}, so nothing was installed."
        )));
    }
    Err(Error::Jwt(format!(
        "Record {record_id} was not found in registry {registry_id}, or you are not entitled to it."
    )))
}

/// `str(value or '')`.
fn py_str_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) if !v.is_truthy() => String::new(),
        Some(Value::Str(s)) => s.clone(),
        Some(v) => crate::pyfmt::py_str(v),
    }
}

/// `_download(url, destination, ca_bundle)`.
fn download(url: &str, destination: &str, ca_bundle: Option<&str>) -> Result<()> {
    let origin = url_origin(url);
    let redact = |t: &str| crate::secret_redaction::redact_secrets(t, &Default::default());
    let fail = |e: UrlError| -> Error {
        match e {
            UrlError::Http(e) => Error::Jwt(format!(
                "Download from {origin} failed with HTTP {}.{}",
                e.code,
                if e.code == 403 {
                    " The download URL may have expired; run the command again."
                } else {
                    ""
                }
            )),
            UrlError::Url { reason, .. } => Error::Jwt(format!(
                "Download from {origin} failed: {}",
                redact(&printable(&reason))
            )),
            UrlError::Jwt(m) => Error::Jwt(m),
            e @ UrlError::Os { .. } => {
                let text = redact(&printable(&e.message()));
                Error::Jwt(format!(
                    "Download from {origin} failed: {}",
                    if text.is_empty() {
                        e.type_name().to_string()
                    } else {
                        text
                    }
                ))
            }
        }
    };
    let mut response = urlreq::urlopen(
        &Request {
            url,
            data: None,
            headers: &[],
        },
        Duration::from_secs(HTTP_TIMEOUT),
        "download URL",
        ca_bundle,
    )
    .map_err(fail)?;
    let mut file = std::fs::File::create(destination).map_err(|e| {
        Error::Jwt(format!(
            "Download from {origin} failed: {}",
            redact(&printable(&crate::cli::paramfile::os_error(
                &e,
                destination
            )))
        ))
    })?;
    let mut written: u64 = 0;
    loop {
        let chunk = response.read(CHUNK_BYTES).map_err(fail)?;
        if chunk.is_empty() {
            break;
        }
        written += chunk.len() as u64;
        if written > MAX_ARCHIVE_BYTES {
            return Err(Error::Jwt(format!(
                "Archive exceeds {MAX_ARCHIVE_BYTES} bytes; refusing to continue."
            )));
        }
        file.write_all(&chunk).map_err(|e| {
            Error::Jwt(format!(
                "Download from {origin} failed: {}",
                printable(&crate::cli::paramfile::os_error(&e, destination))
            ))
        })?;
    }
    Ok(())
}

const INSTALL: &str = "install";
const UPDATE: &str = "update";
const REINSTALL: &str = "reinstall";
const UP_TO_DATE: &str = "up to date";
const KEPT: &str = "kept";
const LEFT_AS_IT_IS: [&str; 2] = [UP_TO_DATE, KEPT];

/// `_outcome(action, name, destination, installed, source)`.
fn outcome(
    action: &str,
    name: &str,
    destination: &str,
    installed: Option<&Marker>,
    source: &Marker,
) -> String {
    match action {
        UP_TO_DATE => format!("{name} is already up to date at {destination}."),
        KEPT => format!(
            "{name} at {destination} holds the record's package; the record now publishes no downloadable package, so it was left as it is. Pass --force to replace it with the SKILL.md text only."
        ),
        UPDATE => {
            let installed = installed.expect("an installed marker");
            let mut old = format!(
                "{}\u{2026}",
                printable(&installed.sha256)
                    .chars()
                    .take(12)
                    .collect::<String>()
            );
            let mut new = format!(
                "{}\u{2026}",
                source.sha256.chars().take(12).collect::<String>()
            );
            if installed.content != source.content {
                old = format!("{} {old}", content_label(installed));
                new = format!("{} {new}", content_label(source));
            }
            format!("Updated {name} at {destination} ({old} \u{2192} {new}).")
        }
        REINSTALL => format!("Reinstalled {name} at {destination}."),
        _ => format!("Installed {name} to {destination}"),
    }
}

/// `_content_label(marker)`.
fn content_label(marker: &Marker) -> String {
    content_label_of(&marker.content)
        .map(str::to_string)
        .unwrap_or_else(|| printable(&crate::pyfmt::str_repr(&marker.content)))
}

/// `_newer_install(destination)`.
fn newer_install(destination: &str) -> String {
    format!(
        "Refusing to replace {destination}: it was installed by a newer version of the AWS CLI, which this one does not understand. Update the AWS CLI to install or update the skill here."
    )
}

/// `_same_file(first, second)`.
fn same_file(first: &str, second: &str) -> bool {
    match (std::fs::metadata(first), std::fs::metadata(second)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// `_report_earlier_copies(agent, copies, destination, folder, agent_id)`.
fn report_earlier_copies(
    agent: Option<&str>,
    copies: &[(String, String, Marker)],
    destination: &str,
    folder: &str,
    agent_id: Option<&str>,
) {
    for (earlier, path, _) in copies {
        if lexists(path) && !same_file(path, destination) {
            report(
                agent,
                &format!(
                    "An earlier install of this record is at {path}; uninstall it with {} if you no longer need it.",
                    uninstall_command(earlier, folder, agent_id)
                ),
            );
        }
    }
}

/// `shlex.quote(s)`.
fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// `_uninstall_command(name, folder, agent_id)`.
fn uninstall_command(name: &str, folder: &str, agent_id: Option<&str>) -> String {
    let mut command = vec!["aws", "agent-registry", "uninstall-skill", "--name", name];
    match agent_id {
        Some(id) => command.extend(["--agent", id]),
        None => command.extend(["--target-directory", folder]),
    }
    command
        .iter()
        .map(|p| shlex_quote(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_plan_destination(destination, name, force, source, uninstall)`.
fn plan_destination(
    destination: &str,
    force: bool,
    source: &Marker,
    uninstall: &str,
) -> std::result::Result<(&'static str, Option<Marker>), PlanError> {
    if islink(destination) {
        return Err(PlanError::Refused(format!(
            "Refusing to replace {destination}: it is a symbolic link."
        )));
    }
    if !lexists(destination) {
        return Ok((INSTALL, None));
    }
    if !is_installed_skill(destination) {
        return Err(PlanError::Refused(format!(
            "Refusing to replace {destination}: it has no {SKILL_MARKER} at its root, so it is not an installed skill."
        )));
    }
    let installed = read_provenance(destination);
    if let Some(i) = &installed
        && !is_known_content(i)
    {
        return Err(PlanError::Refused(newer_install(destination)));
    }
    if let Some(i) = &installed
        && same_source(i, source)
    {
        if keeps_package(i, source) && !force {
            return Ok((KEPT, installed));
        }
        if !same_content(i, source) {
            return Ok((UPDATE, installed));
        }
        return Ok((if force { REINSTALL } else { UP_TO_DATE }, installed));
    }
    let elsewhere = "install into another folder with --agent or --target-directory";
    let Some(installed) = installed else {
        return Err(PlanError::Refused(format!(
            "Refusing to replace {destination}: it has no readable {PROVENANCE_MARKER}, so this command can't tell where it came from. Move or delete the folder yourself, or {elsewhere}."
        )));
    };
    let with_endpoint =
        comparable_endpoint(&installed.endpoint) != comparable_endpoint(&source.endpoint);
    Err(PlanError::Refused(format!(
        "Refusing to replace {destination}: it was installed from {}, not {}. Uninstall it first with {uninstall}, or {elsewhere}.",
        source_detail(&installed, with_endpoint),
        source_detail(source, with_endpoint)
    )))
}

/// `_resolve_record_name(client, registry_id, name)`.
fn resolve_record_name(client: &Client<'_>, registry_id: &str, name: &str) -> Result<String> {
    let mut matches: Vec<(String, Option<Value>)> = Vec::new();
    for page in skill_record_pages(client, registry_id)? {
        for record in page {
            if record.get("name") == Some(&Value::str(name))
                && let Some(id) = str_of(&record, "recordId").filter(|i| !i.is_empty())
                && !matches.iter().any(|(m, _)| m == id)
            {
                matches.push((id.to_string(), record.get("recordVersion").cloned()));
            }
        }
    }
    let shown_registry = printable(registry_id);
    let shown_name = printable(&crate::pyfmt::str_repr(name));
    if matches.len() == 1 {
        return Ok(matches.remove(0).0);
    }
    if !matches.is_empty() {
        let found: Vec<String> = matches
            .iter()
            .map(|(id, version)| match version {
                Some(Value::Str(v)) if !v.is_empty() => {
                    format!("{} ({})", printable(id), printable(v))
                }
                _ => printable(id),
            })
            .collect();
        return Err(Error::Jwt(format!(
            "Registry {shown_registry} has more than one skill record named {shown_name}: {}. Pass --record-id to choose one.",
            found.join(", ")
        )));
    }
    Err(Error::Jwt(format!(
        "Registry {shown_registry} has no skill record named {shown_name}; --record-name matches the record's name (name in list-skills), not its SKILL.md name (skillName). Run aws agent-registry list-skills --registry-id {shown_registry} to see the names."
    )))
}

/// `_record_display_name(record, record_id)`.
fn record_display_name(record: &Value, record_id: &str) -> String {
    match str_of(record, "name").filter(|n| !n.is_empty()) {
        Some(n) => printable(n),
        None => printable(record_id),
    }
}

/// `_note_record_name(record_name, name)`.
fn note_record_name(record_name: Option<&Value>, name: &str) {
    if let Some(Value::Str(r)) = record_name
        && !r.is_empty()
        && r != name
    {
        write_status(&format!(
            "Record {} installs as skill {}.",
            printable(&crate::pyfmt::str_repr(r)),
            printable(&crate::pyfmt::str_repr(name))
        ));
    }
}

/// `str.rpartition('/record/')[2]`.
fn after_record(id: &str) -> (bool, &str) {
    match id.rfind("/record/") {
        Some(i) => (true, &id[i + "/record/".len()..]),
        None => (false, id),
    }
}

/// `_record_id(record, record_id)`.
fn record_id_of(record: &Value, record_id: &str) -> String {
    match str_of(record, "recordId").filter(|r| !r.is_empty()) {
        Some(r) => r.to_string(),
        None => after_record(record_id).1.to_string(),
    }
}

/// `_is_record(record, record_id)`.
fn is_record(record: &Value, record_id: &str) -> bool {
    let wanted = Value::str(record_id);
    if record.get("recordId") == Some(&wanted) || record.get("recordArn") == Some(&wanted) {
        return true;
    }
    let (found, bare) = after_record(record_id);
    found && record.get("recordId") == Some(&Value::str(bare))
}

/// `_read_content(record, record_id)`.
fn read_content(record: &Value, record_id: &str) -> Result<(Payload, String)> {
    let shown = printable(record_id);
    let artifact = match record.get("artifact") {
        Some(a @ Value::Map(_)) => a.clone(),
        _ => Value::map(),
    };
    if let Some(url) = artifact.get("downloadUrl").filter(|u| u.is_truthy()) {
        let digest = match artifact.get("sha256") {
            Some(Value::Str(d)) if SHA256_HEX.is_match(d) => d.clone(),
            _ => {
                return Err(Error::Jwt(format!(
                    "Record {shown} does not publish a valid SHA-256 digest for its package (64 lowercase hexadecimal characters), so the download cannot be verified."
                )));
            }
        };
        let url = match url {
            Value::Str(s) => s.clone(),
            v => crate::pyfmt::py_str(v),
        };
        return Ok((Payload::Package(url), digest));
    }
    let skill_md = skill_md_descriptor(record);
    if let Some(Value::Str(data)) = skill_md.get("data")
        && !py_strip(data).is_empty()
    {
        let bytes = encoded_skill_md(data).map_err(|problem| {
            Error::Jwt(format!("Refusing to install record {shown}: {problem}."))
        })?;
        let digest = hex::encode(Sha256::digest(&bytes));
        return Ok((Payload::SkillMd(bytes), digest));
    }
    let link = skill_md
        .get("source")
        .and_then(|s| s.get("fromUrl"))
        .and_then(|f| f.get("url"));
    if let Some(Value::Str(link)) = link
        && !link.is_empty()
    {
        return Err(Error::Jwt(format!(
            "Record {shown} publishes its SKILL.md only as a link to {}. install-skill does not fetch from hosts a publisher chooses; the publisher must upload a package or put the SKILL.md text in the record.",
            printable(&url_origin(link))
        )));
    }
    Err(Error::Jwt(format!(
        "Record {shown} has no downloadable package and no SKILL.md text, so there is nothing to install."
    )))
}

/// `_skill_md_descriptor(record)`.
fn skill_md_descriptor(record: &Value) -> Value {
    let mut found = record.clone();
    for key in ["descriptors", "agentSkillsDefinition", "additionalData"] {
        match found.get(key) {
            Some(v @ Value::Map(_)) => found = v.clone(),
            _ => return Value::map(),
        }
    }
    match found.get("skillMd") {
        Some(v @ Value::Map(_)) => v.clone(),
        _ => Value::map(),
    }
}

/// `skill_md_name(record)`.
pub(crate) fn skill_md_name(record: &Value) -> Option<String> {
    let descriptor = skill_md_descriptor(record);
    let Some(Value::Str(data)) = descriptor.get("data") else {
        return None;
    };
    if py_strip(data).is_empty() {
        return None;
    }
    let encoded = encoded_skill_md(data).ok()?;
    let name = frontmatter_name(&encoded, "the record's SKILL.md").ok()?;
    folder_name_problem(&name).is_none().then_some(name)
}

/// `_encoded_skill_md(data)`.
fn encoded_skill_md(data: &str) -> std::result::Result<Vec<u8>, String> {
    if char_len(data) > MAX_SKILL_MD_CHARACTERS {
        return Err(
            "its SKILL.md text has more than the 102,400 characters a record's SKILL.md can hold"
                .to_string(),
        );
    }
    if data.contains('\0') {
        return Err("its SKILL.md text contains a NUL character".to_string());
    }
    Ok(data.as_bytes().to_vec())
}

/// `_sha256(path)`.
fn sha256_file(path: &str) -> std::result::Result<String, String> {
    let data = std::fs::read(path).map_err(|e| crate::cli::paramfile::os_error(&e, path))?;
    Ok(hex::encode(Sha256::digest(&data)))
}

/// `_collision_key(name)`.
fn collision_key(name: &str) -> String {
    let normalized = posix_normpath(&name.replace('\\', "/"));
    let nfc: String = normalized.nfc().collect();
    casefold(&nfc)
}

/// `str.casefold()`.
pub(crate) fn casefold(s: &str) -> String {
    s.chars()
        .flat_map(
            |c| match super::casefold::CASEFOLD.binary_search_by_key(&c, |(k, _)| *k) {
                Ok(i) => super::casefold::CASEFOLD[i].1.chars().collect::<Vec<_>>(),
                Err(_) => c.to_lowercase().collect(),
            },
        )
        .collect()
}

/// `posixpath.normpath(path)`.
fn posix_normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = comps.join("/");
    let out = format!("{}{joined}", "/".repeat(initial));
    if out.is_empty() { ".".to_string() } else { out }
}

/// `_damaged(error)`.
fn damaged(error: &str) -> Error {
    Error::Jwt(format!(
        "The archive is damaged or uses an unsupported zip feature: {}",
        printable(error)
    ))
}

fn open_error(e: OpenError) -> Error {
    match e {
        OpenError::Zip(z) => damaged(&z.0),
        OpenError::Other(m) => Error::Other(m),
    }
}

/// `_checked_entries(zf)`.
fn checked_entries(zf: &ZipFile) -> Result<Vec<ZipInfo>> {
    let infos = zf.infolist().to_vec();
    if infos.len() > MAX_ENTRIES {
        return Err(Error::Jwt(format!(
            "Archive contains {} entries, more than the {MAX_ENTRIES} allowed.",
            infos.len()
        )));
    }
    let total: u64 = infos.iter().map(|i| i.file_size).sum();
    if total > MAX_EXTRACTED_BYTES {
        return Err(Error::Jwt(format!(
            "Archive expands to {total} bytes, more than the {MAX_EXTRACTED_BYTES} allowed."
        )));
    }
    let mut names = HashSet::new();
    for info in &infos {
        let name = &info.filename;
        let r = crate::pyfmt::str_repr(name);
        if name.starts_with('/') {
            return Err(Error::Jwt(format!(
                "Refusing absolute path in archive: {r}"
            )));
        }
        let normalized = name.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').collect();
        if segments.contains(&"..") {
            return Err(Error::Jwt(format!(
                "Refusing path traversal in archive: {r}"
            )));
        }
        if UNPORTABLE_CHARACTERS.is_match(name) {
            return Err(Error::Jwt(format!(
                "Refusing entry whose name is not a portable file name: {r}"
            )));
        }
        if let Some(reserved) = reserved_name(&segments) {
            let why = reserved_names()
                .iter()
                .find(|(n, _)| *n == reserved)
                .map(|(_, w)| *w)
                .unwrap_or_default();
            return Err(Error::Jwt(format!(
                "Refusing entry {r}: the name {reserved} is reserved for {why}."
            )));
        }
        if (info.external_attr >> 16) & 0xF000 == 0xA000 {
            return Err(Error::Jwt(format!("Refusing symlink in archive: {r}")));
        }
        if info.flag_bits & 0x1 != 0 {
            return Err(Error::Jwt(format!(
                "Refusing encrypted entry in archive: {r}"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(Error::Jwt(format!(
                "Refusing duplicate entry in archive: {r}"
            )));
        }
    }
    check_skill_md_casing(&infos)?;
    check_collisions(infos.iter().map(|i| i.filename.as_str()))?;
    Ok(infos)
}

/// `_check_collisions(names)`.
fn check_collisions<'a>(names: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut keys = HashSet::new();
    for name in names {
        if !keys.insert(collision_key(name)) {
            return Err(Error::Jwt(format!(
                "Refusing entry whose path collides with another entry: {}",
                crate::pyfmt::str_repr(name)
            )));
        }
    }
    Ok(())
}

/// `_normalised(name)`.
fn normalised(name: &str) -> String {
    let name = name.replace('\\', "/");
    let path = name
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/");
    if !path.is_empty() && name.ends_with('/') {
        format!("{path}/")
    } else {
        path
    }
}

/// `_not_a_skill_package(message)`.
fn not_a_skill_package(message: &str) -> Error {
    Error::Jwt(format!("Not a valid skill package: {message}"))
}

/// `_check_skill_md_casing(infos)`.
fn check_skill_md_casing(infos: &[ZipInfo]) -> Result<()> {
    let mut casings: Vec<(String, Vec<String>)> = Vec::new();
    for info in infos {
        let n = normalised(&info.filename);
        let (directory, base) = match n.rfind('/') {
            Some(i) => (n[..i].to_string(), n[i + 1..].to_string()),
            None => (String::new(), n.clone()),
        };
        if casefold(&base) == casefold(SKILL_MARKER) {
            match casings.iter_mut().find(|(d, _)| *d == directory) {
                Some((_, s)) => {
                    if !s.contains(&base) {
                        s.push(base);
                    }
                }
                None => casings.push((directory, vec![base])),
            }
        }
    }
    for (_, spellings) in &casings {
        if spellings.len() > 1 {
            return Err(not_a_skill_package(
                "The package spells SKILL.md in more than one casing in one directory.",
            ));
        }
        if !spellings
            .iter()
            .all(|s| SKILL_MD_NAMES.contains(&s.as_str()))
        {
            return Err(not_a_skill_package(
                "The package spells SKILL.md in a casing other than SKILL.md or skill.md.",
            ));
        }
    }
    Ok(())
}

/// `_install_plan(infos)`: each entry with the name it is installed
/// under (`None` to skip it).
fn install_plan(infos: &[ZipInfo]) -> Result<Vec<(ZipInfo, Option<String>)>> {
    let mut top_levels: Vec<String> = Vec::new();
    let mut candidates: HashSet<String> = HashSet::new();
    let mut enclosed = false;
    for info in infos {
        let path = normalised(&info.filename);
        if path.is_empty() {
            continue;
        }
        let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        if !top_levels.contains(&segments[0].to_string()) {
            top_levels.push(segments[0].to_string());
        }
        if path.ends_with('/') {
            continue;
        }
        if segments.len() == 1 && SKILL_MD_NAMES.contains(&path.as_str()) {
            candidates.insert(path.clone());
        } else if segments.len() == 2 && SKILL_MD_NAMES.contains(&segments[1]) {
            candidates.insert(path.clone());
            enclosed = true;
        }
    }
    let preferred = |folder: &str| {
        SKILL_MD_NAMES
            .iter()
            .map(|s| format!("{folder}{s}"))
            .find(|c| candidates.contains(c))
    };
    let mut skill_md = preferred("");
    if skill_md.is_some() && enclosed {
        return Err(not_a_skill_package(
            "The package has a SKILL.md at its root and another at the top of a directory.",
        ));
    }
    if skill_md.is_none() && top_levels.len() > 1 {
        return Err(not_a_skill_package(
            "The package has more than one top-level entry and no SKILL.md at its root.",
        ));
    }
    if skill_md.is_none()
        && let Some(top) = top_levels.first()
    {
        skill_md = preferred(&format!("{top}/"));
    }
    let Some(skill_md) = skill_md else {
        return Err(not_a_skill_package(
            "The package has no SKILL.md at its root or at the top of a single top-level directory.",
        ));
    };
    let folder = match skill_md.rfind('/') {
        Some(i) => skill_md[..i].to_string(),
        None => String::new(),
    };
    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };
    let mut plan = Vec::new();
    for info in infos {
        let path = normalised(&info.filename);
        let installed = if path == skill_md {
            Some(SKILL_MARKER.to_string())
        } else if !prefix.is_empty() && path.trim_end_matches('/') == folder {
            None
        } else {
            Some(path.get(prefix.len()..).unwrap_or_default().to_string())
        };
        plan.push((info.clone(), installed.filter(|i| !i.is_empty())));
    }
    let names: Vec<String> = plan.iter().filter_map(|(_, n)| n.clone()).collect();
    check_installed_names(&names)?;
    Ok(plan)
}

/// `_check_installed_names(names)`.
fn check_installed_names(names: &[String]) -> Result<()> {
    for name in names {
        let segments: Vec<&str> = name.split('/').collect();
        if segments.contains(&"..")
            || UNPORTABLE_CHARACTERS.is_match(name)
            || reserved_name(&segments).is_some()
        {
            return Err(Error::Jwt(format!(
                "Refusing entry installed as {}: it is not a safe file name.",
                crate::pyfmt::str_repr(name)
            )));
        }
    }
    let unique: HashSet<&String> = names.iter().collect();
    if unique.len() != names.len() {
        return Err(Error::Jwt(
            "Refusing an archive in which two entries install to the same path.".to_string(),
        ));
    }
    check_collisions(names.iter().map(String::as_str))
}

/// `_reserved_name(segments)`.
fn reserved_name(segments: &[&str]) -> Option<&'static str> {
    for segment in segments {
        let folded = casefold(&segment.nfc().collect::<String>());
        for (reserved, _) in reserved_names() {
            if folded == casefold(reserved) {
                return Some(reserved);
            }
        }
    }
    None
}

/// `_check_archive(archive, shown)`: the SKILL.md name.
fn check_archive(archive: &str, shown: &str) -> Result<String> {
    let zf = ZipFile::open(archive).map_err(open_error)?;
    let plan = install_plan(&checked_entries(&zf)?)?;
    let skill_md = plan
        .iter()
        .find(|(_, installed)| installed.as_deref() == Some(SKILL_MARKER))
        .map(|(info, _)| info.clone())
        .expect("a SKILL.md");
    let data = zf
        .read(&skill_md, Some(MAX_SKILL_MD_BYTES + 1))
        .map_err(open_error)?;
    named(frontmatter_name(&data, "the package's SKILL.md"), shown)
}

/// `_named(declared, unreadable, shown)`.
fn named(declared: std::result::Result<String, String>, shown: &str) -> Result<String> {
    match declared {
        Err(unreadable) => Err(Error::Jwt(format!(
            "Refusing to install {shown}: {unreadable}, so the skill's name, which its folder is named after, can't be read."
        ))),
        Ok(declared) => check_declared_name(&declared, shown),
    }
}

/// `_check_declared_name(declared, shown)`.
fn check_declared_name(declared: &str, shown: &str) -> Result<String> {
    if let Some(problem) = folder_name_problem(declared) {
        return Err(Error::Jwt(format!(
            "Refusing to install {shown}: its SKILL.md names the skill {}, which cannot name its folder: {problem}. The publisher must rename the skill in its SKILL.md.",
            printable(&crate::pyfmt::str_repr(declared))
        )));
    }
    if !is_spec_skill_name(declared) {
        write_status(&format!(
            "Warning: {declared} is not an Agent Skills name ({SKILL_NAME_RULE}); some agents may not load it."
        ));
    }
    Ok(declared.to_string())
}

/// `_file_mode(info)`.
fn file_mode(info: &ZipInfo) -> u32 {
    let mut mode = (info.external_attr >> 16) & 0o777;
    if mode == 0 {
        mode = 0o644;
    }
    (mode & 0o755) | 0o600
}

fn os_err(e: std::io::Error, path: &str) -> Error {
    Error::Other(crate::cli::paramfile::os_error(&e, path))
}

/// `os.makedirs(path, exist_ok=True)`.
fn makedirs(path: &str) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) => Err(os_err(e, path)),
    }
}

/// `_extract_file(zf, info, target, budget)`.
fn extract_file(zf: &ZipFile, info: &ZipInfo, target: &str, budget: u64) -> Result<u64> {
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .map_err(|e| os_err(e, target))?;
    let data = zf.read(info, None).map_err(open_error)?;
    let written = data.len() as u64;
    if written > info.file_size {
        return Err(Error::Jwt(format!(
            "Refusing entry {}: it expands beyond the {} bytes its header declares.",
            crate::pyfmt::str_repr(&info.filename),
            info.file_size
        )));
    }
    if written > budget {
        return Err(Error::Jwt(format!(
            "Archive expands beyond the {MAX_EXTRACTED_BYTES} bytes allowed."
        )));
    }
    out.write_all(&data).map_err(|e| os_err(e, target))?;
    drop(out);
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(file_mode(info)))
        .map_err(|e| os_err(e, target))?;
    Ok(written)
}

/// `_safe_extract(archive, destination)`.
fn safe_extract(archive: &str, destination: &str) -> Result<()> {
    makedirs(destination)?;
    let real_destination = realpath(destination);
    let zf = ZipFile::open(archive).map_err(open_error)?;
    let mut targets = Vec::new();
    for (info, installed) in install_plan(&checked_entries(&zf)?)? {
        let Some(installed) = installed else {
            continue;
        };
        let target = realpath(&join(destination, &installed));
        if target != real_destination && !target.starts_with(&format!("{real_destination}/")) {
            return Err(Error::Jwt(format!(
                "Refusing entry that escapes the target directory: {}",
                crate::pyfmt::str_repr(&info.filename)
            )));
        }
        targets.push((info, target));
    }
    let mut budget = MAX_EXTRACTED_BYTES;
    for (info, target) in &targets {
        if info.is_dir() {
            makedirs(target)?;
            continue;
        }
        makedirs(&dirname(target))?;
        budget -= extract_file(&zf, info, target, budget)?;
    }
    chmod_dirs(destination)?;
    Ok(())
}

/// `os.walk(destination)` with `os.chmod(directory, 0o755)`.
fn chmod_dirs(top: &str) -> Result<()> {
    std::fs::set_permissions(top, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| os_err(e, top))?;
    let Ok(entries) = std::fs::read_dir(top) else {
        return Ok(());
    };
    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| join(top, &e.file_name().to_string_lossy()))
        .collect();
    dirs.sort();
    for d in dirs {
        chmod_dirs(&d)?;
    }
    Ok(())
}

/// `remove_leftover(path)`.
pub(crate) fn remove_leftover(path: &str) {
    if let Err(e) = rmtree(path) {
        write_status(&format!(
            "Warning: could not remove {path} ({}). Remove it by hand.",
            printable(&e)
        ));
    }
}

/// `shutil.rmtree(path)`: `Err` is `str(OSError)`.
pub(crate) fn rmtree(path: &str) -> std::result::Result<(), String> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| crate::cli::paramfile::os_error(&e, path))?;
    if meta.file_type().is_symlink() {
        return Err("Cannot call rmtree on a symbolic link".to_string());
    }
    std::fs::remove_dir_all(path).map_err(|e| crate::cli::paramfile::os_error(&e, path))
}

/// `_write_skill_md(staged, data)`.
fn write_skill_md(staged: &str, data: &[u8]) -> Result<()> {
    let path = join(staged, SKILL_MARKER);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .map_err(|e| os_err(e, &path))?;
    f.write_all(data).map_err(|e| os_err(e, &path))?;
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| os_err(e, &path))?;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| os_err(e, staged))?;
    Ok(())
}

/// `_write_provenance(skill_dir, source)`.
fn write_provenance(skill_dir: &str, source: &Marker) -> Result<()> {
    let path = join(skill_dir, PROVENANCE_MARKER);
    let mut m = Map::new();
    m.insert("registryId".into(), Value::str(&source.registry_id));
    m.insert("recordId".into(), Value::str(&source.record_id));
    m.insert("endpoint".into(), Value::str(&source.endpoint));
    m.insert("sha256".into(), Value::str(&source.sha256));
    m.insert("content".into(), Value::str(&source.content));
    let text = format!("{}\n", crate::pyfmt::json_dumps_default(&Value::Map(m)));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .map_err(|e| os_err(e, &path))?;
    f.write_all(text.as_bytes()).map_err(|e| os_err(e, &path))?;
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| os_err(e, &path))?;
    Ok(())
}

/// `_install(populate, name, destination, source, force, uninstall)`.
fn install(
    populate: Populate<'_>,
    name: &str,
    destination: &str,
    force: bool,
    source: &Marker,
    uninstall: &str,
) -> std::result::Result<(&'static str, Option<Marker>), PlanError> {
    let parent = dirname(destination);
    makedirs(&parent).map_err(|e| PlanError::Failed(e.message()))?;
    let staged = mkdtemp(&parent, &format!(".{name}.staging-")).map_err(PlanError::Failed)?;
    let result = (|| {
        populate(&staged).map_err(|e| PlanError::Failed(e.message()))?;
        if !std::path::Path::new(&join(&staged, SKILL_MARKER)).is_file() {
            return Err(PlanError::Failed(format!(
                "The archive for {name} has no {SKILL_MARKER} at its root, so it is not a valid skill package."
            )));
        }
        write_provenance(&staged, source).map_err(|e| PlanError::Failed(e.message()))?;
        let (action, installed) = plan_destination(destination, force, source, uninstall)?;
        if !LEFT_AS_IT_IS.contains(&action) {
            replace(&staged, destination).map_err(PlanError::Failed)?;
        }
        Ok((action, installed))
    })();
    if lexists(&staged) {
        remove_leftover(&staged);
    }
    result
}

/// `_replace(staged, destination)`: `Err` is `str(OSError)`.
fn replace(staged: &str, destination: &str) -> std::result::Result<(), String> {
    let mut backup = None;
    if lexists(destination) {
        let token: [u8; 4] = rand::random();
        let b = join(
            &dirname(destination),
            &format!(".{}.old-{}", basename(destination), hex::encode(token)),
        );
        std::fs::rename(destination, &b).map_err(|e| rename_error(&e, destination, &b))?;
        backup = Some(b);
    }
    if let Err(e) = std::fs::rename(staged, destination) {
        if let Some(b) = &backup
            && let Err(r) = std::fs::rename(b, destination)
        {
            write_status(&format!(
                "Warning: could not restore the previous install to {destination} ({}). It is in {b}.",
                printable(&rename_error(&r, b, destination))
            ));
        }
        return Err(rename_error(&e, staged, destination));
    }
    if let Some(b) = backup {
        remove_leftover(&b);
    }
    Ok(())
}

/// `str(OSError)` of `os.rename(src, dst)`.
fn rename_error(e: &std::io::Error, src: &str, dst: &str) -> String {
    format!(
        "{} -> {}",
        crate::cli::paramfile::os_error(e, src),
        crate::pyfmt::str_repr(dst)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_hosts() {
        for (host, region, expected) in [
            ("bucket.s3.us-west-2.amazonaws.com", "us-west-2", true),
            ("s3.dualstack.us-west-2.amazonaws.com", "us-west-2", true),
            ("b.s3-us-west-2.amazonaws.com", "us-west-2", true),
            ("b.s3.amazonaws.com", "us-east-1", true),
            ("b.s3.amazonaws.com", "us-west-2", false),
            ("evil.example", "us-east-1", false),
            ("b.s3.us-east-1.amazonaws.com.cn", "us-east-1", true),
        ] {
            assert_eq!(is_s3_host(host, region), expected, "{host} {region}");
        }
    }

    /// `testdata/archives.json` (`gen_archives.py`): `_check_archive` and
    /// `_safe_extract` of the reference on crafted packages.
    #[test]
    fn archives() {
        use base64::Engine as _;
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("testdata/archives.json")).unwrap();
        let base = std::env::temp_dir().join(format!("awsr-archives-{}", std::process::id()));
        for (i, case) in cases.as_array().unwrap().iter().enumerate() {
            let name = case["name"].as_str().unwrap();
            let dir = base.join(i.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let archive = dir.join("package.zip").to_string_lossy().into_owned();
            let data = base64::engine::general_purpose::STANDARD
                .decode(case["zip"].as_str().unwrap())
                .unwrap();
            std::fs::write(&archive, data).unwrap();
            let out = dir.join("out").to_string_lossy().into_owned();
            let result = check_archive(&archive, "shown")
                .and_then(|skill| safe_extract(&archive, &out).map(|()| skill));
            match (result, case.get("error")) {
                (Err(e), Some(expected)) => {
                    assert_eq!(e.message(), expected.as_str().unwrap(), "{name}")
                }
                (Ok(skill), None) => {
                    assert_eq!(skill, case["skill"].as_str().unwrap(), "{name}");
                    assert_eq!(listing(&out), case["files"], "{name}");
                }
                (r, expected) => panic!("{name}: {:?} vs {expected:?}", r.map_err(|e| e.message())),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The `listing` of `gen_archives.py`: each folder of `os.walk` in
    /// sorted order, its folders then its files, with modes and contents.
    fn listing(root: &str) -> serde_json::Value {
        fn dirs(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            out.push(d.to_path_buf());
            for e in std::fs::read_dir(d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    dirs(&e.path(), out);
                }
            }
        }
        let root = std::path::Path::new(root);
        let mut all = Vec::new();
        dirs(root, &mut all);
        all.sort_by_key(|p| p.to_string_lossy().into_owned());
        let mut out = Vec::new();
        for d in all {
            let mut entries: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            let rel =
                |p: &std::path::Path| p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let mode = |p: &std::path::Path| {
                format!(
                    "0o{:o}",
                    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
                )
            };
            for e in entries.iter().filter(|e| e.file_type().unwrap().is_dir()) {
                out.push(serde_json::json!([
                    format!("{}/", rel(&e.path())),
                    mode(&e.path()),
                    null
                ]));
            }
            for e in entries.iter().filter(|e| !e.file_type().unwrap().is_dir()) {
                let text = String::from_utf8_lossy(&std::fs::read(e.path()).unwrap()).into_owned();
                out.push(serde_json::json!([rel(&e.path()), mode(&e.path()), text]));
            }
        }
        serde_json::Value::Array(out)
    }

    #[test]
    fn quoting() {
        assert_eq!(shlex_quote("pdf-tool"), "pdf-tool");
        assert_eq!(shlex_quote("/a b"), "'/a b'");
        assert_eq!(shlex_quote("it's"), "'it'\"'\"'s'");
    }
}
