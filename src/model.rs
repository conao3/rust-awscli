//! Access to the embedded botocore models.
//!
//! Each service is one JSON document (`models/services-*/<service>.json.zst`,
//! produced by `scripts/vendor-models.sh`) with the keys `api_version`,
//! `service` (`service-2.json`), `paginators` (the `pagination` object of
//! `paginators-1.json`), `waiters` (the `waiters` object of
//! `waiters-2.json`), and `ruleset` (`endpoint-rule-set-1.json`), which
//! `build.rs` embeds in the binary as a [`crate::bjson`] document read in
//! place.

use anyhow::{Result, anyhow};

use crate::bjson::{Node, Obj};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/models.rs"));
}

/// The version of the AWS CLI whose models are embedded.
pub const REFERENCE_VERSION: &str = include_str!(concat!(env!("OUT_DIR"), "/VERSION"));

/// Names of every embedded botocore service, sorted.
pub fn service_names() -> impl Iterator<Item = &'static str> {
    embedded::SERVICES.iter().map(|(name, _)| *name)
}

pub fn has_service(name: &str) -> bool {
    embedded::SERVICES.iter().any(|(n, _)| *n == name)
}

pub fn partitions() -> &'static Node {
    Node::from_bytes(embedded::PARTITIONS)
}

pub fn endpoints() -> &'static Node {
    Node::from_bytes(embedded::ENDPOINTS)
}

fn credential_scopes() -> &'static Node {
    Node::from_bytes(embedded::CREDENTIAL_SCOPES)
}

/// `credentialScope.service` of the endpoint
/// `EndpointResolver.construct_endpoint` resolves in `endpoints.json`
/// (the service's endpoint for `region`, or its `partitionEndpoint` when
/// it is not regionalized or no region is given).
fn legacy_credential_scope_service(service: &str, region: Option<&str>) -> Option<String> {
    let partitions = credential_scopes()["partitions"].as_array()?;
    if !partitions
        .iter()
        .any(|p| p["services"].get(service).is_some())
    {
        return None;
    }
    let resolve = |partition: &Node, force: bool| -> Option<Option<String>> {
        let service_data = partition["services"].get(service);
        let endpoints = service_data.and_then(|s| s.get("endpoints"));
        let partition_endpoint = service_data
            .and_then(|s| s.get("partitionEndpoint"))
            .and_then(|e| e.as_str());
        let region = match region {
            Some(r) => r,
            None => partition_endpoint?,
        };
        let endpoint_name = if endpoints.and_then(|e| e.get(region)).is_some() {
            region
        } else {
            let matches = partition["regions"].get(region).is_some()
                || partition["regionRegex"]
                    .as_str()
                    .and_then(|re| regex::Regex::new(re).ok())
                    .is_some_and(|re| re.is_match(region));
            if !matches && !force {
                return None;
            }
            let regionalized = service_data
                .and_then(|s| s.get("isRegionalized"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match partition_endpoint {
                Some(e) if !regionalized => e,
                _ => region,
            }
        };
        let scope = |data: Option<&Node>| {
            data.and_then(|d| d["credentialScope"]["service"].as_str())
                .map(str::to_string)
        };
        Some(
            scope(endpoints.and_then(|e| e.get(endpoint_name)))
                .or_else(|| scope(service_data.and_then(|s| s.get("defaults")))),
        )
    };
    partitions
        .iter()
        .find_map(|p| resolve(p, false))
        .or_else(|| {
            partitions
                .iter()
                .find(|p| p["partition"] == "aws")
                .and_then(|p| resolve(p, true))
        })
        .flatten()
}

/// `awscli/data/cli.json` (the global options).
pub fn cli_data() -> &'static Node {
    Node::from_bytes(embedded::CLI)
}

/// One botocore service model.
#[derive(Debug)]
pub struct ServiceModel {
    /// The botocore service name (the data directory name, e.g. `s3`).
    pub name: String,
    pub api_version: String,
    doc: &'static Node,
}

impl ServiceModel {
    pub fn load(name: &str) -> Result<ServiceModel> {
        let (_, bytes) = embedded::SERVICES
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or_else(|| anyhow!("unknown service: {name}"))?;
        let doc = Node::from_bytes(bytes);
        let api_version = doc["api_version"].as_str().unwrap_or_default().to_string();
        Ok(ServiceModel {
            name: name.to_string(),
            api_version,
            doc,
        })
    }

    #[cfg(test)]
    pub fn load_source(name: &str) -> ServiceModel {
        let path = [awsr_models_services_a_l::DIR, awsr_models_services_m_z::DIR]
            .iter()
            .map(|dir| format!("{dir}/{name}.json.zst"))
            .find(|path| std::path::Path::new(path).exists())
            .unwrap();
        let raw = zstd::decode_all(std::fs::File::open(path).unwrap()).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        ServiceModel {
            name: name.to_string(),
            api_version: doc["api_version"].as_str().unwrap_or_default().to_string(),
            doc: Node::leak(&doc),
        }
    }

    /// A model with only `shapes` (the argument models
    /// `awscli.arguments.create_argument_model_from_schema` resolves).
    pub fn from_shapes(name: &str, shapes: serde_json::Value) -> ServiceModel {
        ServiceModel {
            name: name.to_string(),
            api_version: String::new(),
            doc: Node::leak(&serde_json::json!({
                "service": {"metadata": {}, "operations": {}, "shapes": shapes}
            })),
        }
    }

    fn service(&self) -> &'static Node {
        &self.doc["service"]
    }

    /// `ServiceModel.documentation`.
    pub fn documentation(&self) -> &str {
        self.service()
            .get("documentation")
            .and_then(Node::as_str)
            .unwrap_or_default()
    }

    pub fn metadata(&self) -> &Obj {
        self.service()["metadata"].as_object().expect("metadata")
    }

    pub fn metadata_str(&self, key: &str) -> Option<&str> {
        self.metadata().get(key).and_then(Node::as_str)
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
        if let Some(list) = self.metadata().get("protocols").and_then(Node::as_array) {
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

    /// The client's signing name (`ClientEndpointBridge._resolve_signing_name`):
    /// the `credentialScope` service of the `endpoints.json` endpoint for
    /// `region`, else [`ServiceModel::signing_name`].
    pub fn client_signing_name(&self, region: Option<&str>) -> String {
        legacy_credential_scope_service(self.endpoint_prefix(), region)
            .unwrap_or_else(|| self.signing_name().to_string())
    }

    pub fn cli_operation_names(&self) -> Vec<(&str, &str)> {
        match (
            self.service()["operations"].as_object(),
            self.doc["cli_operation_names"].as_array(),
        ) {
            (Some(ops), Some(names)) => ops
                .keys()
                .zip(names.iter().filter_map(Node::as_str))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Operation names in model order.
    pub fn operation_names(&self) -> Vec<&str> {
        self.service()["operations"]
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default()
    }

    pub fn operation(&self, name: &str) -> Option<Operation<'_>> {
        let def = self.service()["operations"].get(name)?.as_object()?;
        Some(Operation {
            model: self,
            name: self.service()["operations"]
                .as_object()?
                .get_key_value(name)?
                .0,
            def,
        })
    }

    pub fn shape_def(&self, name: &str) -> Option<&Obj> {
        self.service()["shapes"].get(name)?.as_object()
    }

    pub fn shape(&self, name: &str) -> Option<Shape<'_>> {
        let def = self.shape_def(name)?;
        let name = self.service()["shapes"].as_object()?.get_key_value(name)?.0;
        Some(Shape {
            model: self,
            name,
            def,
            member: None,
        })
    }

    /// The `pagination` entry for an operation (`paginators-1.json`).
    pub fn paginator(&self, operation: &str) -> Option<&Obj> {
        self.doc["paginators"].get(operation)?.as_object()
    }

    /// The `waiters` object (`waiters-2.json`), if any.
    pub fn waiters(&self) -> Option<&Obj> {
        self.doc["waiters"].as_object()
    }

    /// `endpoint-rule-set-1.json`, if any.
    pub fn ruleset(&self) -> Option<&Node> {
        let r = &self.doc["ruleset"];
        (!r.is_null()).then_some(r)
    }

    /// The `clientContextParams` of the service.
    pub fn client_context_params(&self) -> Option<&Obj> {
        self.service().get("clientContextParams")?.as_object()
    }
}

/// An operation of a service.
#[derive(Debug, Clone, Copy)]
pub struct Operation<'a> {
    pub model: &'a ServiceModel,
    pub name: &'a str,
    pub def: &'a Obj,
}

impl<'a> Operation<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Node> {
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
            .and_then(Node::as_array)
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

    pub fn http(&self) -> Option<&'a Obj> {
        self.def.get("http")?.as_object()
    }

    pub fn http_method(&self) -> &'a str {
        self.http()
            .and_then(|h| h.get("method"))
            .and_then(Node::as_str)
            .unwrap_or("POST")
    }

    pub fn request_uri(&self) -> &'a str {
        self.http()
            .and_then(|h| h.get("requestUri"))
            .and_then(Node::as_str)
            .unwrap_or("/")
    }

    pub fn is_deprecated(&self) -> bool {
        self.def
            .get("deprecated")
            .and_then(Node::as_bool)
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
    pub def: &'a Obj,
    /// The member reference (`{"shape": ..., "location": ...}`) this shape
    /// was reached through, if any.
    pub member: Option<&'a Obj>,
}

impl<'a> Shape<'a> {
    pub fn type_name(&self) -> &'a str {
        self.def
            .get("type")
            .and_then(Node::as_str)
            .unwrap_or("structure")
    }

    /// A trait looked up on the member reference first, then on the shape.
    pub fn get(&self, key: &str) -> Option<&'a Node> {
        self.member
            .and_then(|m| m.get(key))
            .or_else(|| self.def.get(key))
    }

    pub fn get_str(&self, key: &str) -> Option<&'a str> {
        self.get(key).and_then(Node::as_str)
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key).and_then(Node::as_bool).unwrap_or(false)
    }

    /// A trait on the member reference only.
    pub fn member_get(&self, key: &str) -> Option<&'a Node> {
        self.member.and_then(|m| m.get(key))
    }

    /// A trait on the shape definition only.
    pub fn shape_get(&self, key: &str) -> Option<&'a Node> {
        self.def.get(key)
    }

    fn resolve_ref(&self, r: &'a Node) -> Option<Shape<'a>> {
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
            .and_then(Node::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, r)| Some((k, self.resolve_ref(r)?)))
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
            .and_then(Node::as_array)
            .map(|r| r.iter().filter_map(Node::as_str).collect())
            .unwrap_or_default()
    }

    pub fn is_document(&self) -> bool {
        self.def
            .get("document")
            .and_then(Node::as_bool)
            .unwrap_or(false)
    }

    pub fn is_union(&self) -> bool {
        self.def
            .get("union")
            .and_then(Node::as_bool)
            .unwrap_or(false)
    }

    pub fn is_event_stream(&self) -> bool {
        self.def
            .get("eventstream")
            .and_then(Node::as_bool)
            .unwrap_or(false)
    }

    pub fn is_streaming(&self) -> bool {
        self.get_bool("streaming")
    }

    pub fn enum_values(&self) -> Vec<&'a str> {
        self.def
            .get("enum")
            .and_then(Node::as_array)
            .map(|r| r.iter().filter_map(Node::as_str).collect())
            .unwrap_or_default()
    }
}

impl ServiceModel {
    /// Shape names in model order (`botocore/model.py`
    /// `ServiceModel.shape_names`).
    pub fn shape_names(&self) -> Vec<&str> {
        self.service()["shapes"]
            .as_object()
            .map(|o| o.keys().collect())
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
