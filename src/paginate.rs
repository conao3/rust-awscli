//! Pagination (`botocore/paginate.py`, `awscli/customizations/paginate.py`).

use anyhow::Result;

use crate::value::{Map, Value};

/// User-facing pagination options (`--starting-token`, `--max-items`,
/// `--page-size`).
#[derive(Debug, Clone, Default)]
pub struct PaginationConfig {
    pub starting_token: Option<String>,
    pub max_items: Option<i64>,
    pub page_size: Option<i64>,
}

/// The fully consumed result of a `PageIterator`, carrying what the
/// formatters read from it.
#[derive(Debug, Clone)]
pub struct PaginatedResponse {
    /// Each page as parsed (including `ResponseMetadata`), truncated the way
    /// `PageIterator` truncates when `MaxItems` is reached.
    pub pages: Vec<Value>,
    /// The `result_key` JMESPath expressions.
    pub result_keys: Vec<String>,
    /// `PageIterator.non_aggregate_part`.
    pub non_aggregate_part: Map,
    /// `PageIterator.resume_token` (already encoded).
    pub resume_token: Option<String>,
    /// `PageIterator.build_full_result()`.
    pub full_result: Value,
}

/// Issues one API call with the given parameters and returns the parsed
/// response.
pub type CallFn<'a> = dyn FnMut(&Value) -> Result<Value> + 'a;

/// Run a paginator over `call`, starting from `params`, according to the
/// `pagination` entry of `paginators-1.json` and `config`.
pub fn paginate(
    paginator: &serde_json::Map<String, serde_json::Value>,
    params: &Value,
    config: &PaginationConfig,
    call: &mut CallFn<'_>,
) -> Result<PaginatedResponse> {
    let _ = (paginator, params, config, call);
    todo!("paginate")
}
