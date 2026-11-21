//! `awscli/customizations/agentregistry/sign_out.py`: revoke and delete
//! cached `--authorizer JWT` sign-ins.

use super::basic::{self, ArgDef};
use super::cache::{self, Pending};
use super::constants::MAX_PRINTED_TEXT;
use super::urlreq::UrlError;
use super::utils::{
    FetchError, bare_registry_id, comparable_endpoint, http_post_form, printable,
    printable_or_name, refuse_endpoint_query, require_https_jwt, resolve_ca_bundle,
    resolve_data_plane_endpoint, str_of, write_status,
};
use super::{Error, Result};
use crate::driver::Context;
use crate::error::ExitCode;
use crate::value::Value;

/// `_ERROR_BODY_BYTES`.
const ERROR_BODY_BYTES: usize = 4 * MAX_PRINTED_TEXT;

/// `_not_revocable(entry)`.
fn not_revocable(entry: &Value) -> Option<&'static str> {
    if !entry.get("refreshToken").is_some_and(Value::is_truthy) {
        return Some("there is no refresh token");
    }
    if str_of(entry, "revocationEndpoint").is_none_or(str::is_empty) {
        return Some("the identity provider does not advertise a revocation endpoint");
    }
    None
}

/// `str(value)` of an entry's field (`None` when it is missing).
fn field_str(entry: &Value, key: &str) -> String {
    match entry.get(key) {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => crate::pyfmt::py_str(v),
        None => "None".to_string(),
    }
}

/// `_revoke(entry, ca_bundle)`: why the request failed, or `None`.
fn revoke(entry: &Value, ca_bundle: Option<&str>) -> Option<String> {
    let endpoint = field_str(entry, "revocationEndpoint");
    let token = field_str(entry, "refreshToken");
    let client_id = match entry.get("clientId") {
        Some(v) if v.is_truthy() => field_str(entry, "clientId"),
        _ => String::new(),
    };
    let result = http_post_form(
        &endpoint,
        &[
            ("token", &token),
            ("token_type_hint", "refresh_token"),
            ("client_id", &client_id),
        ],
        ca_bundle,
        false,
    );
    match result {
        Ok(_) => None,
        Err(FetchError::Url(UrlError::Http(e))) => {
            let body = printable(&e.read_error_body(ERROR_BODY_BYTES));
            let detail = if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            };
            Some(format!(
                "HTTP {} from the identity provider{detail}",
                e.code
            ))
        }
        Err(FetchError::Url(UrlError::Url { reason, .. })) => {
            Some(printable_or_name(&reason, "URLError"))
        }
        Err(e) => Some(printable_or_name(&e.message(), e.type_name())),
    }
}

/// `_label(entry)`.
fn label(registry_id: &str, endpoint: &str) -> String {
    format!(
        "Registry {} at {}",
        printable(registry_id),
        printable(endpoint)
    )
}

fn entry_label(entry: &Value) -> String {
    label(&or_empty(entry, "registryId"), &or_empty(entry, "endpoint"))
}

/// `str(entry.get(key) or '')`.
fn or_empty(entry: &Value, key: &str) -> String {
    match entry.get(key) {
        Some(v) if v.is_truthy() => field_str(entry, key),
        _ => String::new(),
    }
}

/// `_retry_advice(entry)`.
fn retry_advice(registry_id: &str) -> String {
    format!(
        "run aws agent-registry sign-out --registry-id {} again",
        printable(registry_id)
    )
}

/// `_report_unreadable(path, problem, warning)`.
fn report_unreadable(path: &str, problem: &str, warning: bool) {
    write_status(&format!(
        "{} not read the refresh tokens kept for revocation in {path} ({}). Left it in place: check it, and delete it once its refresh tokens are revoked or have expired.",
        if warning { "Warning: could" } else { "Could" },
        printable(problem)
    ));
}

type PendingFile = (String, Option<Pending>, Option<String>);
type Entry = (String, Option<Value>);

/// `SignOutCommand._run_main`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode> {
    let defs = vec![ArgDef::new("registry-id"), ArgDef::new("all").flag()];
    let Some(a) =
        basic::parse(ctx, &["agent-registry", "sign-out"], &defs, args).map_err(Error::Cli)?
    else {
        return Ok(ExitCode::Success);
    };
    let registry_arg = a.str("registry-id").filter(|r| !r.is_empty());
    let all = a.bool("all");
    if registry_arg.is_some() == all {
        return Err(Error::Param(
            "Specify exactly one of --registry-id and --all.".to_string(),
        ));
    }
    let (pending, entries, others): (Vec<PendingFile>, Vec<Entry>, Vec<String>) = if all {
        let pending = cache::list_pending_revocations();
        let entries = cache::list_entries();
        let others = cache::list_temp_files();
        if pending.is_empty() && entries.is_empty() && others.is_empty() {
            write_status("Not signed in.");
            return Ok(ExitCode::Success);
        }
        (pending, entries, others)
    } else {
        let registry_id = bare_registry_id(registry_arg.as_deref().unwrap_or_default())?;
        let endpoint = endpoint(ctx)?;
        let (pending, elsewhere) = pending_for(&registry_id, endpoint.as_deref());
        let entries = entries_for(&registry_id, endpoint.as_deref());
        for (path, problem) in &elsewhere {
            report_unreadable(path, problem, true);
        }
        if pending.is_empty() && entries.is_empty() {
            let where_ = endpoint.map(|e| format!(" at {e}")).unwrap_or_default();
            write_status(&format!("No cached sign-in for {registry_id}{where_}."));
            return Ok(ExitCode::Success);
        }
        (pending, entries, vec![])
    };

    let ca_bundle = resolve_ca_bundle(ctx);
    let mut ok = true;
    for (path, document, problem) in &pending {
        ok &= retry(
            path,
            document.as_ref(),
            problem.as_deref(),
            ca_bundle.as_deref(),
        );
    }
    for (path, entry) in &entries {
        ok &= sign_out(path, entry.as_ref(), ca_bundle.as_deref());
    }
    for path in &others {
        ok &= remove(path, "the unfinished cache write");
    }
    Ok(if ok {
        ExitCode::Success
    } else {
        ExitCode::FAILURE
    })
}

/// `_endpoint(parsed_globals)`.
fn endpoint(ctx: &Context) -> Result<Option<String>> {
    let Some(endpoint_url) = ctx.globals.endpoint_url.clone().filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    let region = ctx
        .globals
        .region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| ctx.session.config().get("region").ok().flatten())
        .filter(|r| !r.is_empty());
    let Some(region) = region else {
        refuse_endpoint_query(&endpoint_url, &ctx.globals)?;
        let url = require_https_jwt(&endpoint_url, "registry endpoint")?;
        return Ok(Some(url.trim_end_matches('/').to_string()));
    };
    resolve_data_plane_endpoint(ctx, &region, true).map(Some)
}

/// `_entries_for(registry_id, endpoint)`.
fn entries_for(registry_id: &str, endpoint: Option<&str>) -> Vec<(String, Option<Value>)> {
    let wanted = endpoint.map(comparable_endpoint);
    cache::list_entries()
        .into_iter()
        .filter(|(_, entry)| {
            entry.as_ref().is_some_and(|e| {
                str_of(e, "registryId") == Some(registry_id)
                    && wanted.as_ref().is_none_or(|w| {
                        let ep = match e.get("endpoint") {
                            Some(Value::Str(s)) => s.clone(),
                            _ => String::new(),
                        };
                        &comparable_endpoint(&ep) == w
                    })
            })
        })
        .collect()
}

/// `_pending_for(registry_id, endpoint)`: `(pending, elsewhere)`.
fn pending_for(
    registry_id: &str,
    endpoint: Option<&str>,
) -> (Vec<PendingFile>, Vec<(String, String)>) {
    let wanted = endpoint.map(comparable_endpoint);
    let mut pending = Vec::new();
    let mut elsewhere = Vec::new();
    for (path, document, problem) in cache::list_pending_revocations() {
        match &document {
            Some(d) => {
                if d.registry_id == registry_id
                    && wanted
                        .as_ref()
                        .is_none_or(|w| &comparable_endpoint(&d.endpoint) == w)
                {
                    pending.push((path, document, problem));
                }
            }
            None => {
                if cache::pending_revocation_registry(&path).as_deref() == Some(registry_id) {
                    pending.push((path, document, problem));
                } else {
                    elsewhere.push((path, problem.unwrap_or_default()));
                }
            }
        }
    }
    (pending, elsewhere)
}

/// `_retry(path, document, problem, ca_bundle)`.
fn retry(
    path: &str,
    document: Option<&Pending>,
    problem: Option<&str>,
    ca_bundle: Option<&str>,
) -> bool {
    let Some(document) = document else {
        report_unreadable(path, problem.unwrap_or_default(), false);
        return false;
    };
    let label = label(&document.registry_id, &document.endpoint);
    let mut revoked = Vec::new();
    for token in &document.tokens {
        match revoke(token, ca_bundle) {
            None => {
                revoked.push(field_str(token, "refreshToken"));
                write_status(&format!(
                    "{label}: revoked a refresh token that an earlier sign-out could not revoke."
                ));
            }
            Some(reason) => write_status(&format!(
                "{label}: could not revoke a refresh token that an earlier sign-out could not revoke either ({reason}). Kept it so the revocation can be retried: {}.",
                retry_advice(&document.registry_id)
            )),
        }
    }
    if !revoked.is_empty()
        && let Err(e) = cache::remove_pending_revocations(path, &revoked)
    {
        write_status(&format!(
            "{label}: could not remove the revoked refresh tokens from {path}: {}",
            printable(&e)
        ));
        return false;
    }
    revoked.len() == document.tokens.len()
}

/// `_remove(path, what)`.
fn remove(path: &str, what: &str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => {
            write_status(&format!("Removed {what} {path}."));
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            write_status(&format!(
                "Could not remove {what} {path}: {}",
                printable(&crate::cli::paramfile::os_error(&e, path))
            ));
            false
        }
    }
}

/// `_sign_out(path, entry, ca_bundle)`.
fn sign_out(path: &str, entry: Option<&Value>, ca_bundle: Option<&str>) -> bool {
    let (label, revoked) = match entry {
        None => (
            "Unreadable cache entry".to_string(),
            "did not revoke the refresh token (the CLI cannot read the entry)".to_string(),
        ),
        Some(entry) => {
            let label = entry_label(entry);
            let reason = not_revocable(entry);
            if reason.is_none()
                && let Some(failure) = revoke(entry, ca_bundle)
            {
                return keep_for_retry(path, entry, &label, &failure);
            }
            (
                label,
                match reason {
                    None => "revoked the refresh token".to_string(),
                    Some(r) => format!("did not revoke the refresh token ({r})"),
                },
            )
        }
    };
    if !remove_entry(path, &format!("{label}: {revoked}")) {
        return false;
    }
    write_status(&format!("{label}: {revoked}; removed the sign-in."));
    true
}

/// `_keep_for_retry(path, entry, label, failure)`.
fn keep_for_retry(path: &str, entry: &Value, label: &str, failure: &str) -> bool {
    let registry_id = or_empty(entry, "registryId");
    if let Err(e) = cache::add_pending_revocation(entry) {
        write_status(&format!(
            "{label}: could not revoke the refresh token ({failure}), and could not keep it to retry the revocation later ({}). Left the sign-in in place so the revocation can be retried: {}.",
            printable(&e),
            retry_advice(&registry_id)
        ));
        return false;
    }
    if remove_entry(
        path,
        &format!(
            "{label}: could not revoke the refresh token ({failure}); kept it so the revocation can be retried"
        ),
    ) {
        write_status(&format!(
            "{label}: could not revoke the refresh token ({failure}). Removed the sign-in from this machine and kept the refresh token so the revocation can be retried: {}.",
            retry_advice(&registry_id)
        ));
    }
    false
}

/// `_remove_entry(path, outcome)`.
fn remove_entry(path: &str, outcome: &str) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            write_status(&format!(
                "{outcome}; could not remove {path}: {}",
                printable(&crate::cli::paramfile::os_error(&e, path))
            ));
            false
        }
    }
}
