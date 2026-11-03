//! Access to the embedded botocore models.
//!
//! Each service is stored as one zstd-compressed JSON document
//! (`models/services/<service>.json.zst`, produced by
//! `scripts/vendor-models.sh`) with the keys `api_version`, `service`
//! (`service-2.json`), `paginators` (the `pagination` object of
//! `paginators-1.json`), `waiters` (the `waiters` object of
//! `waiters-2.json`), and `ruleset` (`endpoint-rule-set-1.json`).

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use serde_json::{Map as JsonMap, Value as Json};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/models.rs"));
}

static PARTITIONS: &[u8] = include_bytes!("../models/partitions.json.zst");
static ENDPOINTS: &[u8] = include_bytes!("../models/endpoints.json.zst");
static CLI: &[u8] = include_bytes!("../models/cli.json.zst");

/// The version of the AWS CLI whose models are embedded.
pub const REFERENCE_VERSION: &str = include_str!("../models/VERSION");

fn decompress(bytes: &[u8]) -> Result<Json> {
    let raw = zstd::decode_all(bytes).context("decompress embedded model")?;
    serde_json::from_slice(&raw).context("parse embedded model")
}

/// Names of every embedded botocore service, sorted.
pub fn service_names() -> impl Iterator<Item = &'static str> {
    embedded::SERVICES.iter().map(|(name, _)| *name)
}

pub fn has_service(name: &str) -> bool {
    embedded::SERVICES.iter().any(|(n, _)| *n == name)
}

pub fn partitions() -> &'static Json {
    static P: OnceLock<Json> = OnceLock::new();
    P.get_or_init(|| decompress(PARTITIONS).expect("embedded partitions.json"))
}

pub fn endpoints() -> &'static Json {
    static P: OnceLock<Json> = OnceLock::new();
    P.get_or_init(|| decompress(ENDPOINTS).expect("embedded endpoints.json"))
}

/// `awscli/data/cli.json` (the global options).
pub fn cli_data() -> &'static Json {
    static P: OnceLock<Json> = OnceLock::new();
    P.get_or_init(|| decompress(CLI).expect("embedded cli.json"))
}

/// One botocore service model.
#[derive(Debug)]
pub struct ServiceModel {
    /// The botocore service name (the data directory name, e.g. `s3`).
    pub name: String,
    pub api_version: String,
    doc: Json,
}

impl ServiceModel {
    pub fn load(name: &str) -> Result<ServiceModel> {
        let (_, bytes) = embedded::SERVICES
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or_else(|| anyhow!("unknown service: {name}"))?;
        let doc = decompress(bytes)?;
        let api_version = doc["api_version"].as_str().unwrap_or_default().to_string();
        Ok(ServiceModel {
            name: name.to_string(),
            api_version,
            doc,
        })
    }

    fn service(&self) -> &Json {
        &self.doc["service"]
    }

    pub fn metadata(&self) -> &JsonMap<String, Json> {
        self.service()["metadata"].as_object().expect("metadata")
    }

    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata().get(key).and_then(Json::as_str)
    }

    /// The protocol botocore uses for this service: the first entry of
    /// `metadata.protocols` in `PRIORITY_ORDERED_SUPPORTED_PROTOCOLS`
    /// order, or `metadata.protocol`.
    pub fn protocol(&self) -> &str {
        const PRIORITY: [&str; 6] = [
            "json",
            "rest-json",
            "rest-xml",
            "smithy-rpc-v2-cbor",
            "query",
            "ec2",
        ];
        if let Some(list) = self.metadata().get("protocols").and_then(Json::as_array) {
            for p in PRIORITY {
                if list.iter().any(|v| v.as_str() == Some(p)) {
                    return p;
                }
            }
        }
        self.metadata_str("protocol").unwrap_or("json")
    }

    pub fn endpoint_prefix(&self) -> &str {
        self.metadata_str("endpointPrefix").unwrap_or(&self.name)
    }

    pub fn signing_name(&self) -> &str {
        self.metadata_str("signingName")
            .unwrap_or_else(|| self.endpoint_prefix())
    }

    /// Operation names in model order.
    pub fn operation_names(&self) -> Vec<&str> {
        self.service()["operations"]
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn operation(&self, name: &str) -> Option<Operation<'_>> {
        let def = self.service()["operations"].get(name)?.as_object()?;
        Some(Operation {
            model: self,
            name: self.service()["operations"]
                .as_object()?
                .get_key_value(name)?
                .0
                .as_str(),
            def,
        })
    }

    pub fn shape_def(&self, name: &str) -> Option<&JsonMap<String, Json>> {
        self.service()["shapes"].get(name)?.as_object()
    }

    pub fn shape(&self, name: &str) -> Option<Shape<'_>> {
        let def = self.shape_def(name)?;
        let name = self.service()["shapes"]
            .as_object()?
            .get_key_value(name)?
            .0
            .as_str();
        Some(Shape {
            model: self,
            name,
            def,
            member: None,
        })
    }

    /// The `pagination` entry for an operation (`paginators-1.json`).
    pub fn paginator(&self, operation: &str) -> Option<&JsonMap<String, Json>> {
        self.doc["paginators"].get(operation)?.as_object()
    }

    /// The `waiters` object (`waiters-2.json`), if any.
    pub fn waiters(&self) -> Option<&JsonMap<String, Json>> {
        self.doc["waiters"].as_object()
    }

    /// `endpoint-rule-set-1.json`, if any.
    pub fn ruleset(&self) -> Option<&Json> {
        let r = &self.doc["ruleset"];
        (!r.is_null()).then_some(r)
    }

    /// The `clientContextParams` of the service.
    pub fn client_context_params(&self) -> Option<&JsonMap<String, Json>> {
        self.service().get("clientContextParams")?.as_object()
    }
}

/// An operation of a service.
#[derive(Debug, Clone, Copy)]
pub struct Operation<'a> {
    pub model: &'a ServiceModel,
    pub name: &'a str,
    pub def: &'a JsonMap<String, Json>,
}

impl<'a> Operation<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Json> {
        self.def.get(key)
    }

    fn shape_ref(&self, key: &str) -> Option<Shape<'a>> {
        let r = self.def.get(key)?.as_object()?;
        let target = r.get("shape")?.as_str()?;
        let mut s = self.model.shape(target)?;
        s.member = Some(r);
        Some(s)
    }

    pub fn input(&self) -> Option<Shape<'a>> {
        self.shape_ref("input")
    }

    pub fn output(&self) -> Option<Shape<'a>> {
        self.shape_ref("output")
    }

    /// Error shapes in model order.
    pub fn errors(&self) -> Vec<Shape<'a>> {
        self.def
            .get("errors")
            .and_then(Json::as_array)
            .map(|errs| {
                errs.iter()
                    .filter_map(|e| {
                        let target = e.get("shape")?.as_str()?;
                        self.model.shape(target)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn http(&self) -> Option<&'a JsonMap<String, Json>> {
        self.def.get("http")?.as_object()
    }

    pub fn http_method(&self) -> &'a str {
        self.http()
            .and_then(|h| h.get("method"))
            .and_then(Json::as_str)
            .unwrap_or("POST")
    }

    pub fn request_uri(&self) -> &'a str {
        self.http()
            .and_then(|h| h.get("requestUri"))
            .and_then(Json::as_str)
            .unwrap_or("/")
    }

    pub fn is_deprecated(&self) -> bool {
        self.def
            .get("deprecated")
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }
}

/// A shape, optionally seen through the member reference that points at
/// it (botocore merges member-level traits such as `location`,
/// `locationName`, `jsonvalue`, `timestampFormat`, `idempotencyToken`,
/// `hostLabel`, `xmlNamespace`, `xmlAttribute`, `flattened` into the
/// shape's serialization view).
#[derive(Debug, Clone, Copy)]
pub struct Shape<'a> {
    pub model: &'a ServiceModel,
    pub name: &'a str,
    pub def: &'a JsonMap<String, Json>,
    /// The member reference (`{"shape": ..., "location": ...}`) this shape
    /// was reached through, if any.
    pub member: Option<&'a JsonMap<String, Json>>,
}

impl<'a> Shape<'a> {
    pub fn type_name(&self) -> &'a str {
        self.def
            .get("type")
            .and_then(Json::as_str)
            .unwrap_or("structure")
    }

    /// A trait looked up on the member reference first, then on the shape.
    pub fn get(&self, key: &str) -> Option<&'a Json> {
        self.member
            .and_then(|m| m.get(key))
            .or_else(|| self.def.get(key))
    }

    pub fn get_str(&self, key: &str) -> Option<&'a str> {
        self.get(key).and_then(Json::as_str)
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key).and_then(Json::as_bool).unwrap_or(false)
    }

    /// A trait on the member reference only.
    pub fn member_get(&self, key: &str) -> Option<&'a Json> {
        self.member.and_then(|m| m.get(key))
    }

    /// A trait on the shape definition only.
    pub fn shape_get(&self, key: &str) -> Option<&'a Json> {
        self.def.get(key)
    }

    fn resolve_ref(&self, r: &'a Json) -> Option<Shape<'a>> {
        let r = r.as_object()?;
        let target = r.get("shape")?.as_str()?;
        let mut s = self.model.shape(target)?;
        s.member = Some(r);
        Some(s)
    }

    /// Structure members in model order.
    pub fn members(&self) -> Vec<(&'a str, Shape<'a>)> {
        self.def
            .get("members")
            .and_then(Json::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, r)| Some((k.as_str(), self.resolve_ref(r)?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn member_shape(&self, name: &str) -> Option<Shape<'a>> {
        let r = self.def.get("members")?.get(name)?;
        self.resolve_ref(r)
    }

    /// The `member` of a list shape.
    pub fn list_member(&self) -> Option<Shape<'a>> {
        self.resolve_ref(self.def.get("member")?)
    }

    pub fn map_key(&self) -> Option<Shape<'a>> {
        self.resolve_ref(self.def.get("key")?)
    }

    pub fn map_value(&self) -> Option<Shape<'a>> {
        self.resolve_ref(self.def.get("value")?)
    }

    pub fn required(&self) -> Vec<&'a str> {
        self.def
            .get("required")
            .and_then(Json::as_array)
            .map(|r| r.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default()
    }

    pub fn is_document(&self) -> bool {
        self.def
            .get("document")
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    pub fn is_union(&self) -> bool {
        self.def
            .get("union")
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    pub fn is_event_stream(&self) -> bool {
        self.def
            .get("eventstream")
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    pub fn is_streaming(&self) -> bool {
        self.get_bool("streaming")
    }

    pub fn enum_values(&self) -> Vec<&'a str> {
        self.def
            .get("enum")
            .and_then(Json::as_array)
            .map(|r| r.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default()
    }
}

impl ServiceModel {
    /// The operation model changes botocore makes before the first call
    /// when endpoint rule sets are in effect: s3
    /// `remove_bucket_from_url_paths_from_model` and s3control
    /// `remove_accid_host_prefix_from_model` (`botocore/handlers.py`).
    pub fn apply_endpoint_ruleset_customizations(&mut self) {
        let name = self.name.clone();
        let Some(ops) = self.doc["service"]["operations"].as_object_mut() else {
            return;
        };
        for op in ops.values_mut() {
            if name == "s3" {
                if let Some(uri) = op.get_mut("http").and_then(|h| h.get_mut("requestUri"))
                    && let Some(rest) = uri.as_str().and_then(|u| u.strip_prefix("/{Bucket}"))
                {
                    *uri = Json::String(rest.to_string());
                }
            } else if name == "s3control" {
                let requires_account_id = op
                    .get("staticContextParams")
                    .and_then(|p| p.get("RequiresAccountId"))
                    .and_then(|p| p.get("value"))
                    .and_then(Json::as_bool)
                    == Some(true);
                if requires_account_id
                    && let Some(endpoint) = op.get_mut("endpoint").and_then(Json::as_object_mut)
                    && endpoint.get("hostPrefix").and_then(Json::as_str) == Some("{AccountId}.")
                {
                    endpoint.remove("hostPrefix");
                }
            }
        }
    }

    /// Shape names in model order (`botocore/model.py`
    /// `ServiceModel.shape_names`).
    pub fn shape_names(&self) -> Vec<&str> {
        self.service()["shapes"]
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_every_embedded_service() {
        let names: Vec<_> = service_names().collect();
        assert!(names.len() > 400);
        let sts = ServiceModel::load("sts").unwrap();
        assert_eq!(sts.protocol(), "query");
        let op = sts.operation("GetCallerIdentity").unwrap();
        let out = op.output().unwrap();
        let members: Vec<_> = out.members().into_iter().map(|(n, _)| n).collect();
        assert_eq!(members, ["UserId", "Account", "Arn"]);
    }

    #[test]
    fn protocol_priority() {
        let m = ServiceModel::load("dynamodb").unwrap();
        assert_eq!(m.protocol(), "json");
    }
}
