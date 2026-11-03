//! Request handlers of `botocore/handlers.py` for single services:
//! `base64_encode_user_data` (ec2, autoscaling), `fix_route53_ids`,
//! `switch_host_machinelearning`, and `change_get_to_post`
//! (cloudsearchdomain).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::model::Operation;
use crate::session::url::{SplitUrl, urlsplit, urlunsplit};
use crate::value::{Map, Value};

/// `base64_encode_user_data`.
pub(super) fn base64_encode_user_data(params: &mut Map) {
    let encoded = match params.get("UserData") {
        Some(Value::Str(s)) => STANDARD.encode(s.as_bytes()),
        Some(Value::Bytes(b)) => STANDARD.encode(b),
        _ => return,
    };
    params.insert("UserData".to_string(), Value::Str(encoded));
}

/// `fix_route53_ids`: the last `/`-separated piece of the members whose
/// shape is `ResourceId`, `DelegationSetId`, or `ChangeId`.
pub(super) fn fix_route53_ids(operation: &Operation<'_>, params: &mut Map) {
    let Some(input) = operation.input() else {
        return;
    };
    for (name, shape) in input.members() {
        if !matches!(shape.name, "ResourceId" | "DelegationSetId" | "ChangeId") {
            continue;
        }
        if let Some(Value::Str(v)) = params.get_mut(name)
            && let Some(last) = v.rsplit('/').next()
        {
            *v = last.to_string();
        }
    }
}

/// `utils._get_new_endpoint` with `use_new_scheme=True`: the scheme and
/// host of `new_endpoint` with the path and query of `original`.
pub(super) fn switch_host(original: &str, new_endpoint: &str) -> String {
    let new = urlsplit(new_endpoint);
    let orig = urlsplit(original);
    urlunsplit(&SplitUrl {
        scheme: new.scheme,
        netloc: new.netloc,
        path: orig.path,
        query: orig.query,
        fragment: String::new(),
    })
}

/// `switch_host_machinelearning` (`switch_host_with_param(request,
/// 'PredictEndpoint')`): the new URL, if the JSON body names an endpoint.
pub(super) fn switch_host_machinelearning(url: &str, body: &[u8]) -> Option<String> {
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let endpoint = body.get("PredictEndpoint")?;
    let truthy = match endpoint {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    };
    if !truthy {
        return None;
    }
    Some(switch_host(url, endpoint.as_str()?))
}

/// `change_get_to_post`: the URL without the query and the query as the
/// form-encoded body, when a GET request has a query.
pub(super) fn change_get_to_post(method: &str, url: &str) -> Option<(String, Vec<u8>)> {
    if method != "GET" {
        return None;
    }
    let (base, query) = url.split_once('?')?;
    Some((base.to_string(), query.as_bytes().to_vec()))
}
