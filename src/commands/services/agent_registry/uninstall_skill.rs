//! `awscli/customizations/agentregistry/uninstall_skill.py`: uninstall a
//! skill that `install-skill` put in place (local only).

use std::os::unix::fs::MetadataExt as _;

use super::basic::{self, ArgDef};
use super::install_skill::{
    MAX_SCANNED_ENTRIES, PROVENANCE_MARKER, SKILL_MARKER, agent_argument, basename,
    describe_source, dirname, islink, lexists, read_provenance, report, resolve_skill_folders,
    rmtree, safe_folder_name, skill_path,
};
use super::utils::{printable, prompt, stdin_is_terminal, write_status};
use super::{Error, Result};
use crate::driver::Context;
use crate::error::ExitCode;

/// `_require_installed_skill(path, name)`.
fn require_installed_skill(path: &str, name: &str) -> Result<()> {
    if islink(path) {
        return Err(Error::Jwt(format!(
            "Refusing to remove {path}: it is a symbolic link."
        )));
    }
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(Error::Jwt(format!(
            "No skill named {name} is installed at {path}."
        )));
    }
    if !p.is_dir() {
        return Err(Error::Jwt(format!(
            "Refusing to remove {path}: it is not a directory."
        )));
    }
    if !std::path::Path::new(&format!("{path}/{SKILL_MARKER}")).is_file() {
        return Err(Error::Jwt(format!(
            "Refusing to remove {path}: it has no {SKILL_MARKER} at its root, so it does not look like an installed skill."
        )));
    }
    Ok(())
}

use super::install_skill::casefold as fold;

/// `_on_disk_path(path)`.
fn on_disk_path(path: &str) -> String {
    let parent = dirname(path);
    let name = basename(path);
    let Ok(info) = std::fs::symlink_metadata(path) else {
        return path.to_string();
    };
    if info.ino() == 0 {
        return path.to_string();
    }
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return path.to_string();
    };
    let mut found = None;
    for (count, entry) in entries.enumerate() {
        if count >= MAX_SCANNED_ENTRIES {
            break;
        }
        let Ok(entry) = entry else {
            return path.to_string();
        };
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        if entry_name == name {
            return path.to_string();
        }
        if found.is_none()
            && fold(&entry_name) == fold(name)
            && let Ok(m) = std::fs::symlink_metadata(entry.path())
            && m.dev() == info.dev()
            && m.ino() == info.ino()
        {
            found = Some(entry.path().to_string_lossy().into_owned());
        }
    }
    found.unwrap_or_else(|| path.to_string())
}

/// `UninstallSkillCommand._run_main`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode> {
    let defs = vec![
        ArgDef::new("name").required(),
        agent_argument(),
        ArgDef::new("target-directory"),
        ArgDef::new("yes").flag(),
    ];
    let Some(a) = basic::parse(ctx, &["agent-registry", "uninstall-skill"], &defs, args)
        .map_err(Error::Cli)?
    else {
        return Ok(ExitCode::Success);
    };
    let name = safe_folder_name(&a.str("name").unwrap_or_default())?;
    let folders = resolve_skill_folders(ctx, &a)?;

    let mut found = Vec::new();
    let mut failed = false;
    for (agent, folder, _) in &folders {
        let checked = skill_path(folder, &name).and_then(|path| {
            if !lexists(&path) {
                return Ok(None);
            }
            require_installed_skill(&path, &name)?;
            Ok(Some(on_disk_path(&path)))
        });
        let path = match checked {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                report(agent.as_deref(), &e.message());
                failed = true;
                continue;
            }
        };
        let Some(marker) = read_provenance(&path) else {
            report(
                agent.as_deref(),
                &format!(
                    "Skipping {path}: it has no readable {PROVENANCE_MARKER}, so it wasn't installed by aws agent-registry install-skill. Delete it yourself if you no longer need it."
                ),
            );
            failed = true;
            continue;
        };
        found.push((agent.clone(), path, marker));
    }
    if found.is_empty() {
        if failed {
            return Ok(ExitCode::FAILURE);
        }
        let where_ = folders
            .iter()
            .map(|(_, f, _)| f.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::Jwt(if folders.is_empty() {
            format!("No skill named {name} is installed: no supported AI coding agents were found.")
        } else {
            format!("No skill named {name} is installed in {where_}.")
        }));
    }

    if !a.bool("yes") && !confirmed(&found)? {
        return Ok(ExitCode::FAILURE);
    }

    for (agent, path, marker) in &found {
        if islink(path) {
            report(
                agent.as_deref(),
                &format!("Refusing to remove {path}: it is a symbolic link."),
            );
            failed = true;
            continue;
        }
        if let Err(e) = rmtree(path) {
            report(
                agent.as_deref(),
                &format!("Could not remove {path}: {}", printable(&e)),
            );
            failed = true;
            continue;
        }
        report(
            agent.as_deref(),
            &format!(
                "Uninstalled {} from {} (installed {})",
                basename(path),
                dirname(path),
                describe_source(marker)
            ),
        );
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::Success
    })
}

/// `_confirmed(found)`.
fn confirmed(found: &[(Option<String>, String, super::install_skill::Marker)]) -> Result<bool> {
    let paths: Vec<&str> = found.iter().map(|(_, p, _)| p.as_str()).collect();
    if !stdin_is_terminal() {
        return Err(Error::Jwt(format!(
            "Refusing to remove {} without confirmation: standard input is not a terminal. Pass --yes to remove {}.",
            paths.join(", "),
            if paths.len() == 1 { "it" } else { "them" }
        )));
    }
    let question = if let [(_, path, marker)] = found {
        format!("Remove {path} ({})? [y/N]: ", describe_source(marker))
    } else {
        let listing: String = found
            .iter()
            .map(|(_, path, marker)| format!("  {path} ({})\n", describe_source(marker)))
            .collect();
        format!("Remove these {} folders?\n{listing}[y/N]: ", found.len())
    };
    let answer = prompt(&question)?.to_lowercase();
    if answer == "y" || answer == "yes" {
        return Ok(true);
    }
    write_status("Aborted, nothing was removed.");
    Ok(false)
}
