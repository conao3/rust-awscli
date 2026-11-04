//! `agent-toolkit get-skill-file`, `list-installed-skills`, and
//! `remove-skill` (`awscli/customizations/agenttoolkit/`).

use super::basic::{self, ArgDef};
use super::exec::{Client, sub_context};
use crate::driver::{Context, write_stdout};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "get-skill-file" => get_skill_file(ctx, args),
        "list-installed-skills" => list_installed_skills(ctx, args),
        "remove-skill" => remove_skill(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `agenttoolkit.utils.create_client`: the `us-east-1` client unless a
/// region is given on the command line.
fn client_context(ctx: &Context) -> Result<Option<Context>, CliError> {
    if ctx.globals.region.as_deref().is_some_and(|r| !r.is_empty()) {
        return Ok(None);
    }
    sub_context(
        ctx,
        ctx.globals.endpoint_url.clone(),
        Some("us-east-1".to_string()),
        None,
    )
    .map(Some)
}

/// `GetSkillFileCommand._run_main`.
fn get_skill_file(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("skill-name").required().type_name("string"),
        ArgDef::new("file-path").required().type_name("string"),
        ArgDef::new("skill-version").type_name("string"),
    ];
    let Some(a) = basic::parse(ctx, &["agent-toolkit", "get-skill-file"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let own = client_context(ctx)?;
    let client = Client::new(own.as_ref().unwrap_or(ctx), "agenttoolkit")?;
    let version = a
        .str("skill-version")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "latest".to_string());
    let mut params = Map::new();
    params.insert(
        "name".to_string(),
        Value::str(a.str("skill-name").unwrap_or_default()),
    );
    params.insert("skillVersion".to_string(), Value::str(version));
    params.insert(
        "filePath".to_string(),
        Value::str(a.str("file-path").unwrap_or_default()),
    );
    let response = client.call("GetSkillFile", &Value::Map(params))?;
    let body = match response.get("body") {
        Some(Value::Bytes(b)) => b.clone(),
        Some(Value::Str(s)) => s.clone().into_bytes(),
        _ => Vec::new(),
    };
    let text = String::from_utf8(body).map_err(|e| {
        ctx.general_error(&anyhow::anyhow!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
            e.as_bytes()[e.utf8_error().valid_up_to()],
            e.utf8_error().valid_up_to()
        ))
    })?;
    write_stdout(ctx, text.as_bytes())?;
    Ok(ExitCode::Success)
}

const SKILL_FILENAME: &str = "SKILL.md";
const SKILL_METADATA_FILENAME: &str = ".aws-skill-metadata";
const UNIVERSAL_ROW_ID: &str = "universal";

/// `AgentConfig` (the fields the skill commands use).
struct AgentConfig {
    id: &'static str,
    display_name: String,
    detection_path: &'static str,
    detection_path_env_override: Option<&'static str>,
    skills_dir: &'static str,
    skills_path_override: Option<&'static str>,
}

fn config(
    id: &'static str,
    display_name: &str,
    detection_path: &'static str,
    env: Option<&'static str>,
    skills_path_override: Option<&'static str>,
) -> AgentConfig {
    AgentConfig {
        id,
        display_name: display_name.to_string(),
        detection_path,
        detection_path_env_override: env,
        skills_dir: "skills",
        skills_path_override,
    }
}

/// `os.path.expanduser('~')`.
fn home() -> String {
    crate::config::ini::expand_path("~", &|k| std::env::var(k).ok())
}

/// `os.path.expanduser` (`~` and `~/...` only).
fn expanduser(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        let home = home();
        let home = home.trim_end_matches('/');
        let joined = format!("{home}{}", &path[1..]);
        if joined.is_empty() {
            "/".to_string()
        } else {
            joined
        }
    } else {
        path.to_string()
    }
}

fn rstrip_sep(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `AGENT_CONFIGS` with the universal row `_build_universal_row` appends.
fn agent_configs() -> Vec<AgentConfig> {
    let agents_skills = Some("~/.agents/skills/");
    let mut configs = vec![
        config(
            "claude-code",
            "Claude Code",
            "~/.claude/",
            Some("CLAUDE_CONFIG_DIR"),
            None,
        ),
        config("cline", "Cline", "~/.cline/", None, None),
        config(
            "codex",
            "Codex",
            "~/.codex/",
            Some("CODEX_HOME"),
            agents_skills,
        ),
        config("cursor", "Cursor", "~/.cursor/", None, None),
        config("devin", "Devin", "~/.config/devin/", None, agents_skills),
        config(
            "gemini-cli",
            "Gemini CLI",
            "~/.gemini/",
            None,
            agents_skills,
        ),
        config("kiro", "Kiro", "~/.kiro/", None, None),
        config("openclaw", "OpenClaw", "~/.openclaw/", None, None),
        config(
            "opencode",
            "OpenCode",
            "~/.config/opencode/",
            None,
            agents_skills,
        ),
        config("pi", "Pi", "~/.pi/agent/", None, None),
        config(
            "windsurf",
            "Windsurf",
            "~/.codeium/windsurf/",
            None,
            agents_skills,
        ),
    ];
    let universal = rstrip_sep(&expanduser("~/.agents/skills/"));
    let consumers: Vec<String> = configs
        .iter()
        .filter(|c| {
            c.skills_path_override
                .is_some_and(|p| rstrip_sep(&expanduser(p)) == universal)
        })
        .map(|c| c.display_name.clone())
        .collect();
    configs.push(config(
        UNIVERSAL_ROW_ID,
        &format!("Universal ({})", consumers.join(", ")),
        "~/.agents",
        None,
        None,
    ));
    configs
}

/// `DetectedAgent`.
struct DetectedAgent<'c> {
    config: &'c AgentConfig,
    base_dir: String,
}

/// `InstalledSkill`.
struct InstalledSkill<'c> {
    agent: &'c DetectedAgent<'c>,
    name: String,
    path: String,
}

fn is_dir(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

impl AgentConfig {
    /// `resolve_base_dir` (`resolved_override_dir`, then the detection
    /// path).
    fn detect(&self) -> Option<DetectedAgent<'_>> {
        if let Some(var) = self.detection_path_env_override
            && let Ok(value) = std::env::var(var)
            && !value.is_empty()
        {
            let expanded = rstrip_sep(&expanduser(&value));
            if is_dir(&expanded) {
                return Some(DetectedAgent {
                    config: self,
                    base_dir: expanded,
                });
            }
        }
        let expanded = rstrip_sep(&expanduser(self.detection_path));
        is_dir(&expanded).then_some(DetectedAgent {
            config: self,
            base_dir: expanded,
        })
    }
}

/// `collapse_home`.
fn collapse_home(path: &str) -> String {
    let home = rstrip_sep(&home());
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&format!("{home}/")) {
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

impl DetectedAgent<'_> {
    fn skills_path(&self) -> String {
        match self.config.skills_path_override {
            Some(p) => expanduser(p),
            None => format!("{}/{}", self.base_dir, self.config.skills_dir),
        }
    }

    fn display_label(&self) -> String {
        format!(
            "{} \u{2014} {}",
            self.config.display_name,
            collapse_home(&self.skills_path())
        )
    }

    /// `get_installed_skills`: `<skills>/*/SKILL.md` with a metadata file,
    /// in sorted order.
    fn installed_skills(&self) -> Vec<InstalledSkill<'_>> {
        let skills = self.skills_path();
        let Ok(entries) = std::fs::read_dir(&skills) else {
            return Vec::new();
        };
        let base = if skills.ends_with('/') {
            skills.clone()
        } else {
            format!("{skills}/")
        };
        let mut paths: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .map(|n| format!("{base}{n}/{SKILL_FILENAME}"))
            .filter(|p| std::path::Path::new(p).exists())
            .collect();
        paths.sort();
        paths
            .into_iter()
            .filter_map(|path| {
                let dir = path[..path.len() - SKILL_FILENAME.len() - 1].to_string();
                if !std::path::Path::new(&format!("{dir}/{SKILL_METADATA_FILENAME}")).exists() {
                    return None;
                }
                let name = dir.rsplit('/').next().unwrap_or_default().to_string();
                Some(InstalledSkill {
                    agent: self,
                    name,
                    path,
                })
            })
            .collect()
    }
}

/// `universal_first`: a stable sort putting the universal row first.
fn universal_first<T>(items: &mut [T], id: impl Fn(&T) -> &str) {
    items.sort_by_key(|i| if id(i) == UNIVERSAL_ROW_ID { 0 } else { 1 });
}

/// `resolve_agents`.
fn resolve_agents<'c>(
    ctx: &Context,
    configs: &'c [AgentConfig],
    filter: Option<&str>,
) -> Result<Vec<DetectedAgent<'c>>, CliError> {
    match filter.filter(|f| !f.is_empty()) {
        Some(f) => {
            let Some(config) = configs
                .iter()
                .find(|c| c.id.to_lowercase() == f.to_lowercase())
            else {
                let mut ids: Vec<&str> = configs.iter().map(|c| c.id).collect();
                ids.sort();
                return Err(basic::param_validation(
                    ctx,
                    &format!("Invalid agent \"{f}\". Valid values: {}.", ids.join(", ")),
                ));
            };
            Ok(config.detect().into_iter().collect())
        }
        None => Ok(configs.iter().filter_map(AgentConfig::detect).collect()),
    }
}

fn agent_arg() -> ArgDef {
    ArgDef::new("agent").type_name("string")
}

/// `os.path.realpath`.
fn realpath(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// `ListInstalledSkillsCommand._run_main`.
fn list_installed_skills(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![agent_arg()];
    let Some(a) = basic::parse(
        ctx,
        &["agent-toolkit", "list-installed-skills"],
        &defs,
        args,
    )?
    else {
        return Ok(ExitCode::Success);
    };
    let configs = agent_configs();
    let mut agents = resolve_agents(ctx, &configs, a.str("agent").as_deref())?;
    // `collect_installed_skills`.
    universal_first(&mut agents, |a| a.config.id);
    let mut seen = std::collections::HashSet::new();
    let mut skills = Vec::new();
    for agent in &agents {
        for skill in agent.installed_skills() {
            if !seen.insert(realpath(&skill.path)) {
                continue;
            }
            let mut m = Map::new();
            m.insert("agent".into(), Value::str(&skill.agent.config.display_name));
            m.insert("name".into(), Value::str(&skill.name));
            m.insert("path".into(), Value::str(&skill.path));
            skills.push(Value::Map(m));
        }
    }
    let mut result = Map::new();
    result.insert("skills".into(), Value::List(skills));
    crate::driver::display_response_then(
        ctx,
        "list-installed-skills",
        crate::output::Response::Single(Value::Map(result)),
        b"\n",
    )?;
    Ok(ExitCode::Success)
}

/// `RemoveCommand._run_main`.
fn remove_skill(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("skill-name").required().type_name("string"),
        agent_arg(),
    ];
    let Some(a) = basic::parse(ctx, &["agent-toolkit", "remove-skill"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let name = a.str("skill-name").unwrap_or_default();
    let configs = agent_configs();
    let agents = resolve_agents(ctx, &configs, a.str("agent").as_deref())?;
    let mut matches: Vec<InstalledSkill<'_>> = Vec::new();
    for agent in &agents {
        matches.extend(
            agent
                .installed_skills()
                .into_iter()
                .filter(|s| s.name == name),
        );
    }
    if matches.is_empty() {
        return Err(basic::param_validation(
            ctx,
            &format!("Skill \"{name}\" is not installed."),
        ));
    }
    universal_first(&mut matches, |s| s.agent.config.id);
    let mut removed = std::collections::HashSet::new();
    for skill in &matches {
        let dir = skill.path[..skill.path.rfind('/').unwrap_or(0)].to_string();
        if removed.contains(&dir) {
            continue;
        }
        std::fs::remove_dir_all(&dir).map_err(|e| {
            ctx.general_error(&anyhow::anyhow!(crate::cli::paramfile::os_error(&e, &dir)))
        })?;
        removed.insert(dir);
        crate::output::stdout::write_flushed(
            format!(
                "Removed {} from {}.\n",
                skill.name,
                skill.agent.display_label()
            )
            .as_bytes(),
        );
    }
    Ok(ExitCode::Success)
}
