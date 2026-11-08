//! CLI aliases (`awscli/alias.py`): the `[toplevel]` aliases of the main
//! command table (`AliasCommandInjector`) and the `[command <lineage>]`
//! aliases of the subcommand tables (`AliasSubCommandInjector`), read
//! from `~/.aws/cli/alias` (`AliasLoader`).

use std::sync::OnceLock;

use indexmap::IndexMap;

use super::{Fail, GlobalArgs};
use crate::config::ini::{self, LoadError};

/// The alias sections keyed by `'toplevel'` or the command lineage of a
/// `command ...` section.
type Sections = IndexMap<Vec<String>, IndexMap<String, String>>;

static ALIASES: OnceLock<Result<Sections, String>> = OnceLock::new();

/// `AliasLoader._build_aliases`: the file is read once; a file that does
/// not parse fails every command (`ConfigParseError`).
fn sections() -> Result<&'static Sections, Fail> {
    ALIASES
        .get_or_init(load)
        .as_ref()
        .map_err(|e| Fail::General(e.clone()))
}

fn load() -> Result<Sections, String> {
    let path = ini::expand_path("~/.aws/cli/alias", &|k| std::env::var(k).ok());
    let parsed = if std::path::Path::new(&path).exists() {
        match ini::raw_config_parse_flat(&path) {
            Ok(p) => p,
            Err(LoadError::Parse(p) | LoadError::NotFound(p)) => {
                return Err(format!("Unable to parse config file: {p}"));
            }
        }
    } else {
        IndexMap::new()
    };
    let mut out = Sections::new();
    for (key, values) in parsed {
        // `_normalize_key_names`: `command ec2 wait` is the lineage
        // `('command', 'ec2', 'wait')`; other sections keep their name.
        let key: Vec<String> = if key.starts_with("command ") {
            key.split_whitespace().map(str::to_string).collect()
        } else {
            vec![key]
        };
        // `_cleanup_alias_values`.
        let values = values
            .into_iter()
            .map(|(k, v)| (k, v.trim().to_string()))
            .collect();
        out.insert(key, values);
    }
    Ok(out)
}

/// `AliasLoader.get_aliases(command)`: the `[toplevel]` aliases for an
/// empty lineage, else the `[command <lineage>]` ones.
pub(crate) fn get(lineage: &[&str]) -> Result<Vec<(String, String)>, Fail> {
    let key: Vec<String> = if lineage.is_empty() {
        vec!["toplevel".to_string()]
    } else {
        std::iter::once("command")
            .chain(lineage.iter().copied())
            .map(str::to_string)
            .collect()
    };
    Ok(sections()?
        .get(&key)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default())
}

/// The value of the alias `name` of `lineage`.
pub(crate) fn find(lineage: &[&str], name: &str) -> Result<Option<String>, Fail> {
    Ok(get(lineage)?
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v))
}

/// Append the alias names of `lineage` to a command table's names the
/// way the injector adds them to the table (an alias that shadows a
/// command keeps the command's position).
pub(crate) fn extend_choices(lineage: &[&str], choices: &mut Vec<String>) -> Result<(), Fail> {
    for (name, _) in get(lineage)? {
        if !choices.contains(&name) {
            choices.push(name);
        }
    }
    Ok(())
}

/// What running an alias command does next.
#[derive(Debug, Clone)]
pub enum Expansion {
    /// Dispatch `rest` (a command name and its arguments) again with the
    /// updated globals; `shadow` are command paths whose aliases are
    /// skipped from then on (the built-in commands the alias proxies to);
    /// `proxied` is the path of the command the alias replaced in its
    /// command table, if it proxies to it.
    Dispatch {
        globals: Box<GlobalArgs>,
        rest: Vec<String>,
        shadow: Vec<Vec<String>>,
        proxied: Option<Vec<String>>,
    },
    /// `ExternalAliasCommand`: the shell command line.
    External(String),
}

/// `ExternalAliasCommand.__call__`: the alias value without the `!` and
/// the arguments quoted for the shell, with `--help` when it was given
/// (`awscli/commands.py` `call_command`).
pub(crate) fn external(value: &str, args: &[String], help: bool) -> Expansion {
    let mut parts = vec![value[1..].to_string()];
    parts.extend(args.iter().map(|a| shlex_quote(a)));
    if help {
        parts.push("--help".to_string());
    }
    Expansion::External(parts.join(" "))
}

/// `BaseInternalAliasCommand._get_alias_args`.
pub(crate) fn alias_args(name: &str, value: &str) -> Result<Vec<String>, Fail> {
    let args = ini::shlex_split_checked(value).map_err(|e| {
        Fail::General(format!(
            "Value of alias \"{name}\" could not be parsed. Received error: {e} when parsing:\n{value}"
        ))
    })?;
    Ok(args
        .into_iter()
        .map(|a| a.trim_matches('\n').to_string())
        .collect())
}

/// `BaseInternalAliasCommand._update_parsed_globals`: the globals given
/// in the alias (those that differ from the parser defaults) replace the
/// ones parsed from the command line, after the `top-level-args-parsed`
/// handlers ran on them.
pub(crate) fn update_globals(
    name: &str,
    globals: &GlobalArgs,
    alias: &GlobalArgs,
) -> Result<GlobalArgs, Fail> {
    for (unsupported, given) in [("debug", alias.debug), ("profile", alias.profile.is_some())] {
        if given {
            return Err(Fail::General(format!(
                "Global parameter \"--{unsupported}\" detected in alias \"{name}\" which is not supported in subcommand aliases."
            )));
        }
    }
    super::globals::validate(alias)?;
    let mut out = globals.clone();
    out.version |= alias.version;
    out.help |= alias.help;
    fn set<T: Clone + PartialEq>(out: &mut T, value: &T, default: &T) {
        if value != default {
            *out = value.clone();
        }
    }
    let none: Option<String> = None;
    set(&mut out.endpoint_url, &alias.endpoint_url, &none);
    set(&mut out.no_verify_ssl, &alias.no_verify_ssl, &false);
    set(&mut out.no_paginate, &alias.no_paginate, &false);
    set(&mut out.output, &alias.output, &none);
    set(&mut out.query, &alias.query, &none);
    set(&mut out.region, &alias.region, &none);
    set(&mut out.color, &alias.color, &Some("auto".to_string()));
    set(&mut out.no_sign_request, &alias.no_sign_request, &false);
    set(&mut out.ca_bundle, &alias.ca_bundle, &none);
    set(&mut out.cli_read_timeout, &alias.cli_read_timeout, &none);
    set(
        &mut out.cli_connect_timeout,
        &alias.cli_connect_timeout,
        &none,
    );
    set(&mut out.cli_binary_format, &alias.cli_binary_format, &none);
    set(&mut out.no_cli_pager, &alias.no_cli_pager, &false);
    set(&mut out.cli_auto_prompt, &alias.cli_auto_prompt, &false);
    set(
        &mut out.no_cli_auto_prompt,
        &alias.no_cli_auto_prompt,
        &false,
    );
    set(&mut out.cli_error_format, &alias.cli_error_format, &none);
    Ok(out)
}

/// `ServiceAliasCommand.__call__`: the `[toplevel]` alias `name` run with
/// `args`.
pub(crate) fn toplevel(
    name: &str,
    value: &str,
    args: &[String],
    globals: &GlobalArgs,
) -> Result<Expansion, Fail> {
    if value.starts_with('!') {
        return Ok(external(value, args, globals.help));
    }
    let alias_args = alias_args(name, value)?;
    let (alias_globals, mut rest) = super::globals::parse_alias(&alias_args)?;
    let globals = update_globals(name, globals, &alias_globals)?;
    rest.extend(args.iter().cloned());
    // The alias proxies to the command it shadows when its value names
    // that command.
    let shadow =
        if rest.first().is_some_and(|c| c == name) && super::commands::find_main(name).is_some() {
            vec![vec![name.to_string()]]
        } else {
            Vec::new()
        };
    Ok(Expansion::Dispatch {
        globals: Box::new(globals),
        rest,
        shadow,
        proxied: None,
    })
}

/// `InternalAliasSubCommand.__call__` (or `ExternalAliasCommand`): the
/// alias `name` of the command table of `lineage` run with `args`.
/// `proxied` tells whether the alias replaced a command of that table.
pub(crate) fn subcommand(
    lineage: &[&str],
    name: &str,
    value: &str,
    args: &[String],
    globals: &GlobalArgs,
    proxied: bool,
) -> Result<Expansion, Fail> {
    if value.starts_with('!') {
        return Ok(external(value, args, globals.help));
    }
    let alias_args = alias_args(name, value)?;
    // `_process_global_args`: the alias value parsed after
    // `parsed_globals.command` by the main parser.
    let mut parseable = vec![lineage[0].to_string()];
    parseable.extend(alias_args);
    let (alias_globals, rest) = super::globals::parse_alias(&parseable)?;
    let globals = update_globals(name, globals, &alias_globals)?;
    let mut cmd_args: Vec<String> = rest.get(1..).unwrap_or_default().to_vec();
    cmd_args.extend(args.iter().cloned());
    let lineage: Vec<String> = lineage.iter().map(|s| s.to_string()).collect();
    let mut new_rest = lineage.clone();
    // The command object of the table is called directly: the aliases of
    // the tables above it are not looked up again.
    let mut shadow: Vec<Vec<String>> = (1..=lineage.len()).map(|n| lineage[..n].to_vec()).collect();
    let mut proxied_path = None;
    if proxied {
        new_rest.push(name.to_string());
        new_rest.extend(cmd_args.into_iter().skip(1));
        let mut path = lineage;
        path.push(name.to_string());
        shadow.push(path.clone());
        proxied_path = Some(path);
    } else {
        new_rest.extend(cmd_args);
    }
    Ok(Expansion::Dispatch {
        globals: Box::new(globals),
        rest: new_rest,
        shadow,
        proxied: proxied_path,
    })
}

/// `shlex.quote`.
pub(crate) fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote() {
        assert_eq!(shlex_quote("b c"), "'b c'");
        assert_eq!(shlex_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shlex_quote("a-b_c.d"), "a-b_c.d");
        assert_eq!(shlex_quote(""), "''");
    }
}
