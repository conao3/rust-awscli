//! The glacier handlers of `botocore/handlers.py` (`inject_account_id`,
//! `add_glacier_version`, `add_glacier_checksums`) and
//! `botocore/utils.py` `calculate_tree_hash`.

use sha2::{Digest as _, Sha256};

use crate::model::{Operation, ServiceModel};
use crate::protocol::HttpRequest;
use crate::value::{Map, Value};

/// `inject_account_id`.
pub(super) fn inject_account_id(params: &mut Map) {
    if matches!(params.get("accountId"), None | Some(Value::Null)) {
        params.insert("accountId".to_string(), Value::str("-"));
    }
}

/// `before-call.glacier.<Operation>` and `before-call.glacier`.
pub(super) fn before_call(
    model: &ServiceModel,
    operation: &Operation<'_>,
    request: &mut HttpRequest,
) {
    if matches!(operation.name, "UploadArchive" | "UploadMultipartPart") {
        add_glacier_checksums(request);
    }
    super::set_header(
        &mut request.headers,
        "x-amz-glacier-version",
        &model.api_version,
    );
}

/// `add_glacier_checksums`.
fn add_glacier_checksums(request: &mut HttpRequest) {
    let has = |request: &HttpRequest, name: &str| {
        request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    };
    if !has(request, "x-amz-content-sha256") {
        let digest = hex::encode(Sha256::digest(&request.body));
        request
            .headers
            .push(("x-amz-content-sha256".to_string(), digest));
    }
    if !has(request, "x-amz-sha256-tree-hash") {
        let digest = calculate_tree_hash(&request.body);
        request
            .headers
            .push(("x-amz-sha256-tree-hash".to_string(), digest));
    }
}

/// `utils.calculate_tree_hash`: SHA-256 over 1 MiB chunks, combined
/// pairwise until one digest is left.
pub(super) fn calculate_tree_hash(body: &[u8]) -> String {
    let mut chunks: Vec<Vec<u8>> = body
        .chunks(1024 * 1024)
        .map(|c| Sha256::digest(c).to_vec())
        .collect();
    if chunks.is_empty() {
        return hex::encode(Sha256::digest(b""));
    }
    while chunks.len() > 1 {
        chunks = chunks
            .chunks(2)
            .map(|pair| match pair {
                [first, second] => {
                    let mut h = Sha256::new();
                    h.update(first);
                    h.update(second);
                    h.finalize().to_vec()
                }
                [first] => first.clone(),
                _ => unreachable!(),
            })
            .collect();
    }
    hex::encode(&chunks[0])
}
