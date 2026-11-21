//! `awscli/customizations/agentregistry/list_skills.py`: a registry's
//! skills, with the name in each one's SKILL.md and the agents each is
//! installed for.

use std::collections::HashSet;

use super::basic::{self, ArgDef};
use super::dataplane::{create_data_plane_client, sign_in_arguments, skill_record_pages};
use super::install_skill::{
    Folder, agent_argument, installed_skills, resolve_skill_folders, skill_md_name, source_key,
};
use super::utils::{bare_registry_id, printable, printable_limit, str_of, write_status};
use super::{Error, Result};
use crate::commands::services::exec::Client;
use crate::driver::{CallResult, Context};
use crate::error::ExitCode;
use crate::session::{ErrorKind, SessionError};
use crate::value::{Map, Value};

/// `_RECORD_FIELDS`.
const RECORD_FIELDS: [&str; 4] = ["name", "recordId", "description", "recordVersion"];

/// `_BATCH_GET_LIMIT`.
const BATCH_GET_LIMIT: usize = 100;

type SourceKey = (String, String, (String, String, String));

/// `_displayed(skill)`: control characters replaced in string values.
fn displayed(skill: &Value) -> Value {
    match skill {
        Value::Map(m) => Value::Map(
            m.iter()
                .map(|(k, v)| {
                    let v = match v {
                        Value::Str(s) => Value::Str(printable_limit(s, s.chars().count())),
                        v => v.clone(),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        v => v.clone(),
    }
}

/// `ListSkillsCommand._run_main`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode> {
    let mut defs = vec![ArgDef::new("registry-id").required(), agent_argument()];
    defs.extend(sign_in_arguments());
    let Some(a) =
        basic::parse(ctx, &["agent-registry", "list-skills"], &defs, args).map_err(Error::Cli)?
    else {
        return Ok(ExitCode::Success);
    };
    let folders = resolve_skill_folders(ctx, &a)?;
    let registry_arg = a.str("registry-id").unwrap_or_default();
    let registry_id = bare_registry_id(&registry_arg)?;
    let data_plane = create_data_plane_client(ctx, &a)?;
    let client = data_plane.client(ctx)?;

    let installed = installed_index(&folders);
    let mut skills = Vec::new();
    let mut record_ids = Vec::new();
    for page in skill_record_pages(&client, &registry_arg)? {
        for record in page {
            let mut skill = Map::new();
            for field in RECORD_FIELDS {
                if let Some(v) = record.get(field) {
                    skill.insert(field.to_string(), v.clone());
                }
            }
            let record_id = record.get("recordId").cloned();
            let key = match &record_id {
                Some(Value::Str(id)) => source_key(&registry_id, id, &data_plane.endpoint),
                _ => None,
            };
            skill.insert(
                "installedFor".to_string(),
                Value::List(installed_for(key.as_ref(), &installed)),
            );
            skills.push(skill);
            record_ids.push(record_id);
        }
    }

    let names = skill_names(&client, &registry_arg, &record_ids)?;
    let skills: Vec<Value> = skills
        .into_iter()
        .zip(&record_ids)
        .map(|(skill, id)| {
            let name = match id {
                Some(Value::Str(id)) => names.iter().find(|(k, _)| k == id).map(|(_, n)| n),
                _ => None,
            };
            with_skill_name(skill, name)
        })
        .collect();

    let output = ctx
        .globals
        .output
        .clone()
        .or_else(|| ctx.session.config().get("output").ok().flatten());
    let skills: Vec<Value> = if matches!(output.as_deref(), Some("text" | "table")) {
        skills.iter().map(displayed).collect()
    } else {
        skills
    };
    let mut result = Map::new();
    result.insert("skills".to_string(), Value::List(skills));
    crate::driver::display_response(
        ctx,
        "list-skills",
        crate::output::Response::Single(Value::Map(result)),
    )
    .map_err(Error::Cli)?;
    Ok(ExitCode::Success)
}

/// `_installed_index(folders)`: `(agent name, agent id, source keys)`.
fn installed_index(
    folders: &[Folder],
) -> Vec<(Option<String>, Option<String>, HashSet<SourceKey>)> {
    folders
        .iter()
        .map(|(agent, folder, agent_id)| {
            let keys = installed_skills(folder)
                .into_iter()
                .filter_map(|(_, _, m)| source_key(&m.registry_id, &m.record_id, &m.endpoint))
                .collect();
            (agent.clone(), agent_id.clone(), keys)
        })
        .collect()
}

/// `_installed_for(source, installed)`.
fn installed_for(
    key: Option<&SourceKey>,
    installed: &[(Option<String>, Option<String>, HashSet<SourceKey>)],
) -> Vec<Value> {
    let Some(key) = key else {
        return vec![];
    };
    installed
        .iter()
        .filter(|(_, _, sources)| sources.contains(key))
        .map(|(agent, agent_id, _)| {
            let mut m = Map::new();
            m.insert(
                "id".to_string(),
                agent_id.as_deref().map_or(Value::Null, Value::str),
            );
            m.insert(
                "name".to_string(),
                agent.as_deref().map_or(Value::Null, Value::str),
            );
            Value::Map(m)
        })
        .collect()
}

/// The name of the exception a botocore call raised, if `_skill_names`
/// catches it (`ClientError`, `BotoCoreError`,
/// `AgentRegistryJWTError`).
fn caught_name(e: &anyhow::Error) -> Option<&'static str> {
    let se = e.downcast_ref::<SessionError>()?;
    Some(match se.kind {
        ErrorKind::Connection => "EndpointConnectionError",
        ErrorKind::ConnectTimeout => "ConnectTimeoutError",
        ErrorKind::ReadTimeout => "ReadTimeoutError",
        ErrorKind::ConnectionClosed => "ConnectionClosedError",
        ErrorKind::Ssl => "SSLError",
        ErrorKind::Proxy => "ProxyConnectionError",
        ErrorKind::HttpClient => "HTTPClientError",
        ErrorKind::NoCredentials => "NoCredentialsError",
        ErrorKind::PartialCredentials => "PartialCredentialsError",
        ErrorKind::NoAuthToken => "NoAuthTokenError",
        ErrorKind::NoRegion => "NoRegionError",
        ErrorKind::Signature => "UnknownSignatureVersionError",
        ErrorKind::ParamValidation => "ParamValidationError",
        ErrorKind::General if se.message.starts_with("Invalid endpoint configuration") => {
            "InvalidEndpointConfigurationError"
        }
        ErrorKind::General
            if se.message.starts_with("TLS verification can't be disabled")
                || se.message.starts_with("Refusing to use registry endpoint") =>
        {
            "AgentRegistryJWTError"
        }
        _ => return None,
    })
}

/// `_skill_names(client, registry_id, record_ids)`: the SKILL.md name of
/// each record that has one.
fn skill_names(
    client: &Client<'_>,
    registry_id: &str,
    record_ids: &[Option<Value>],
) -> Result<Vec<(String, String)>> {
    let mut wanted: Vec<String> = Vec::new();
    for id in record_ids {
        if let Some(Value::Str(id)) = id
            && !id.is_empty()
            && !wanted.contains(id)
        {
            wanted.push(id.clone());
        }
    }
    let asked: HashSet<&String> = wanted.iter().collect();
    let mut names: Vec<(String, String)> = Vec::new();
    for chunk in wanted.chunks(BATCH_GET_LIMIT) {
        let mut entry = Map::new();
        entry.insert("registryId".to_string(), Value::str(registry_id));
        entry.insert(
            "recordIds".to_string(),
            Value::List(chunk.iter().map(Value::str).collect()),
        );
        let mut params = Map::new();
        params.insert("entries".to_string(), Value::List(vec![Value::Map(entry)]));
        let reason =
            match client.call_raw("BatchGetDiscoverableRegistryRecord", &Value::Map(params)) {
                Ok(CallResult::Ok(response)) => {
                    if let Some(Value::List(records)) = response.get("registryRecords") {
                        for record in records {
                            if !matches!(record, Value::Map(_)) {
                                continue;
                            }
                            let name = skill_md_name(record);
                            if let (Some(name), Some(id)) = (name, str_of(record, "recordId"))
                                && asked.contains(&id.to_string())
                            {
                                match names.iter_mut().find(|(k, _)| k == id) {
                                    Some(slot) => slot.1 = name,
                                    None => names.push((id.to_string(), name)),
                                }
                            }
                        }
                    }
                    continue;
                }
                Ok(CallResult::ServiceError(info)) => {
                    match info.parsed.get("Error").and_then(|e| e.get("Code")) {
                        Some(Value::Str(c)) if !c.is_empty() => c.clone(),
                        _ => "ClientError".to_string(),
                    }
                }
                Err(e) => match caught_name(&e) {
                    Some(name) => name.to_string(),
                    None => return Err(Error::Cli(client.ctx.general_error(&e))),
                },
            };
        write_status(&format!(
            "Warning: could not read the SKILL.md names of the skills ({}), so skillName is left out.",
            printable(&reason)
        ));
        return Ok(vec![]);
    }
    Ok(names)
}

/// `_with_skill_name(skill, name)`.
fn with_skill_name(skill: Map, name: Option<&String>) -> Value {
    let Some(name) = name else {
        return Value::Map(skill);
    };
    let mut result = Map::new();
    if !skill.contains_key("name") {
        result.insert("skillName".to_string(), Value::str(name));
        result.extend(skill);
        return Value::Map(result);
    }
    for (k, v) in skill {
        let is_name = k == "name";
        result.insert(k, v);
        if is_name {
            result.insert("skillName".to_string(), Value::str(name));
        }
    }
    Value::Map(result)
}
