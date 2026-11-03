//! Endpoint resolution: an interpreter for `endpoint-rule-set-1.json` with
//! the standard library and `partitions.json`
//! (`botocore/endpoint_provider.py`, `botocore/regions.py`).

use anyhow::Result;
use indexmap::IndexMap;

/// An endpoint rule set parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Bool(bool),
    StringArray(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub url: String,
    /// `endpoint.properties` as JSON (e.g. `authSchemes`).
    pub properties: serde_json::Map<String, serde_json::Value>,
    /// `endpoint.headers` (each header may have several values).
    pub headers: IndexMap<String, Vec<String>>,
}

/// Evaluate `ruleset` with `params`. An error rule becomes an `Err` whose
/// message is the rule's error string.
pub fn resolve(
    ruleset: &serde_json::Value,
    params: &IndexMap<String, ParamValue>,
) -> Result<Endpoint> {
    let _ = (ruleset, params);
    todo!("endpoint::resolve")
}
