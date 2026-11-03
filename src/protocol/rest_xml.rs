//! The `rest-xml` protocol: `RestXMLSerializer` (`botocore/serialize.py`)
//! and `RestXMLParser` (`botocore/parsers.py`).

use anyhow::{Result, anyhow};
use serde_json::Value as Json;

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::xml_common::{
    self, Flavor, XmlNode, XmlParser, b64encode, float_str, py_str, serialized_name, timestamp_str,
};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::{Map, Value};

/// `RestXMLSerializer._add_xml_namespace`.
fn add_xml_namespace(shape: &Shape<'_>, node: &mut XmlNode) {
    match shape.get("xmlNamespace") {
        Some(Json::Object(ns)) => {
            let mut attribute = String::from("xmlns");
            if let Some(prefix) = ns
                .get("prefix")
                .and_then(Json::as_str)
                .filter(|p| !p.is_empty())
            {
                attribute.push(':');
                attribute.push_str(prefix);
            }
            let uri = ns.get("uri").and_then(Json::as_str).unwrap_or_default();
            node.set_attr(&attribute, uri.to_string());
        }
        Some(Json::String(uri)) => node.set_attr("xmlns", uri.clone()),
        _ => {}
    }
}

/// `RestXMLSerializer._serialize`.
fn serialize_value(
    shape: &Shape<'_>,
    params: &Value,
    parent: &mut XmlNode,
    name: &str,
) -> Result<()> {
    match shape.type_name() {
        "structure" => {
            let node = parent.sub_element(name);
            add_xml_namespace(shape, node);
            let Some(map) = params.as_map() else {
                return Err(anyhow!("expected a structure for {}", shape.name));
            };
            for (key, value) in map {
                let member = shape
                    .member_shape(key)
                    .ok_or_else(|| anyhow!("unknown parameter: {key}"))?;
                let member_name = serialized_name(&member).unwrap_or(key);
                if matches!(value, Value::Null) {
                    return Ok(());
                }
                if member.get_bool("xmlAttribute") {
                    let attr = serialized_name(&member)
                        .ok_or_else(|| anyhow!("xmlAttribute {key} without locationName"))?;
                    node.set_attr(attr, py_str(value));
                    continue;
                }
                serialize_value(&member, value, node, member_name)?;
            }
        }
        "list" => {
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list {} without member", shape.name))?;
            let (element_name, list_node) = if shape.get_bool("flattened") {
                (name.to_string(), parent)
            } else {
                let element_name = serialized_name(&member).unwrap_or("member").to_string();
                (element_name, parent.sub_element(name))
            };
            add_xml_namespace(shape, list_node);
            for item in params.as_list().map(Vec::as_slice).unwrap_or_default() {
                serialize_value(&member, item, list_node, &element_name)?;
            }
        }
        "map" => {
            let key_shape = shape.map_key().ok_or_else(|| anyhow!("map without key"))?;
            let value_shape = shape
                .map_value()
                .ok_or_else(|| anyhow!("map without value"))?;
            let key_name = serialized_name(&key_shape).unwrap_or("key");
            let val_name = serialized_name(&value_shape).unwrap_or("value");
            let flattened = shape.get_bool("flattened");
            let empty = Map::new();
            let entries = params.as_map().unwrap_or(&empty);
            let container: &mut XmlNode = if flattened {
                parent
            } else {
                let node = parent.sub_element(name);
                add_xml_namespace(shape, node);
                node
            };
            for (key, value) in entries {
                let sub = container.sub_element(if flattened { name } else { "entry" });
                serialize_value(&key_shape, &Value::Str(key.clone()), sub, key_name)?;
                serialize_value(&value_shape, value, sub, val_name)?;
            }
        }
        "boolean" => {
            let node = parent.sub_element(name);
            node.text = Some(if params.is_truthy() { "true" } else { "false" }.into());
            add_xml_namespace(shape, node);
        }
        "blob" => {
            let node = parent.sub_element(name);
            node.text = Some(b64encode(params));
            add_xml_namespace(shape, node);
        }
        "timestamp" => {
            let text = timestamp_str(params, shape.get_str("timestampFormat"))?;
            let node = parent.sub_element(name);
            node.text = Some(text);
            add_xml_namespace(shape, node);
        }
        "float" | "double" => {
            let node = parent.sub_element(name);
            node.text = Some(float_str(params));
            add_xml_namespace(shape, node);
        }
        _ => {
            let node = parent.sub_element(name);
            node.text = Some(py_str(params));
            add_xml_namespace(shape, node);
        }
    }
    Ok(())
}

/// `RestXMLSerializer._serialize_body_params`.
fn serialize_body_params(params: &Map, shape: &Shape<'_>) -> Result<Vec<u8>> {
    let root_name = serialized_name(shape)
        .ok_or_else(|| anyhow!("KeyError: 'name' (no locationName on {})", shape.name))?;
    let mut pseudo_root = XmlNode::new("");
    serialize_value(
        shape,
        &Value::Map(params.clone()),
        &mut pseudo_root,
        root_name,
    )?;
    let real_root = pseudo_root
        .children
        .first()
        .ok_or_else(|| anyhow!("IndexError: list index out of range"))?;
    let mut out = String::new();
    real_root.write(&mut out);
    Ok(out.into_bytes())
}

/// `RestXMLSerializer.serialize_to_request` (`BaseRestSerializer`).
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    let _ = model;
    xml_common::rest_serialize(operation, params, &serialize_body_params)
}

/// `RestXMLParser.parse` (plus the endpoint's modeled error fields and
/// `RetryAttempts`).
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let parser = XmlParser {
        flavor: Flavor::RestXml,
        model,
        options: *options,
    };
    parser.parse(operation.output(), response, false)
}

/// `RestXMLParser._do_error_parse` and the modeled error fields.
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let parser = XmlParser {
        flavor: Flavor::RestXml,
        model,
        options: *options,
    };
    Ok(xml_common::service_error(parser.parse(
        operation.output(),
        response,
        true,
    )?))
}

#[cfg(test)]
mod tests {
    //! Goldens produced by botocore (AWS CLI 2.39.3).

    use super::*;
    use crate::protocol::xml_common::test_support::*;

    #[test]
    fn golden_ser_s3_put_object() {
        check_serialize(
            serialize,
            "s3",
            "PutObject",
            r#"{"Bucket": "my-bucket", "Key": "dir/a b+c~ü.txt", "Body": "hello", "ContentType": "text/plain", "Metadata": {"foo": "bar", "Baz": "q x"}, "ACL": "private", "Tagging": "a=b&c=d", "ContentLength": 5, "BucketKeyEnabled": true}"#,
            r#"{"method":"PUT","url_path":"/my-bucket/dir/a%20b%2Bc~%C3%BC.txt","query":[],"headers":[["Content-Type","text/plain"],["x-amz-meta-foo","bar"],["x-amz-meta-Baz","q x"],["x-amz-acl","private"],["x-amz-tagging","a=b&c=d"],["Content-Length","5"],["x-amz-server-side-encryption-bucket-key-enabled","true"]],"body":"hello","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_get_object() {
        check_serialize(
            serialize,
            "s3",
            "GetObject",
            r#"{"Bucket": "b", "Key": "k/../x?y", "Range": "bytes=0-9", "VersionId": "v 1", "PartNumber": 2}"#,
            r#"{"method":"GET","url_path":"/b/k/../x%3Fy","query":[["versionId","v 1"],["partNumber","2"]],"headers":[["Range","bytes=0-9"]],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_list_objects_v2() {
        check_serialize(
            serialize,
            "s3",
            "ListObjectsV2",
            r#"{"Bucket": "b", "Prefix": "a/b c", "MaxKeys": 10, "Delimiter": "/", "FetchOwner": true, "OptionalObjectAttributes": ["RestoreStatus"]}"#,
            r#"{"method":"GET","url_path":"/b?list-type=2","query":[["prefix","a/b c"],["max-keys","10"],["delimiter","/"],["fetch-owner","true"]],"headers":[["x-amz-optional-object-attributes","RestoreStatus"]],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_put_bucket_tagging() {
        check_serialize(
            serialize,
            "s3",
            "PutBucketTagging",
            r#"{"Bucket": "b", "Tagging": {"TagSet": [{"Key": "k<1>", "Value": "a&b\"c"}, {"Key": "e", "Value": ""}]}}"#,
            r#"{"method":"PUT","url_path":"/b?tagging","query":[],"headers":[],"body":"<Tagging xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><TagSet><Tag><Key>k&lt;1&gt;</Key><Value>a&amp;b\"c</Value></Tag><Tag><Key>e</Key><Value /></Tag></TagSet></Tagging>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_put_bucket_versioning() {
        check_serialize(
            serialize,
            "s3",
            "PutBucketVersioning",
            r#"{"Bucket": "b", "MFA": "serial 123456", "VersioningConfiguration": {"MFADelete": "Disabled", "Status": "Enabled"}}"#,
            r#"{"method":"PUT","url_path":"/b?versioning","query":[],"headers":[["x-amz-mfa","serial 123456"]],"body":"<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><MfaDelete>Disabled</MfaDelete><Status>Enabled</Status></VersioningConfiguration>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_create_bucket() {
        check_serialize(
            serialize,
            "s3",
            "CreateBucket",
            r#"{"Bucket": "b", "CreateBucketConfiguration": {"LocationConstraint": "ap-northeast-1"}}"#,
            r#"{"method":"PUT","url_path":"/b","query":[],"headers":[],"body":"<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>ap-northeast-1</LocationConstraint></CreateBucketConfiguration>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_create_bucket_no_config() {
        check_serialize(
            serialize,
            "s3",
            "CreateBucket",
            r#"{"Bucket": "b"}"#,
            r#"{"method":"PUT","url_path":"/b","query":[],"headers":[],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_delete_objects() {
        check_serialize(
            serialize,
            "s3",
            "DeleteObjects",
            r#"{"Bucket": "b", "Delete": {"Objects": [{"Key": "a"}, {"Key": "b", "VersionId": "v"}], "Quiet": true}}"#,
            r#"{"method":"POST","url_path":"/b?delete","query":[],"headers":[],"body":"<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Object><Key>a</Key></Object><Object><Key>b</Key><VersionId>v</VersionId></Object><Quiet>true</Quiet></Delete>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_put_bucket_lifecycle() {
        check_serialize(
            serialize,
            "s3",
            "PutBucketLifecycleConfiguration",
            r#"{"Bucket": "b", "LifecycleConfiguration": {"Rules": [{"ID": "r1", "Status": "Enabled", "Filter": {"Prefix": "logs/"}, "Expiration": {"Days": 30}, "Transitions": [{"Days": 10, "StorageClass": "GLACIER"}]}]}}"#,
            r#"{"method":"PUT","url_path":"/b?lifecycle","query":[],"headers":[],"body":"<LifecycleConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Rule><ID>r1</ID><Status>Enabled</Status><Filter><Prefix>logs/</Prefix></Filter><Expiration><Days>30</Days></Expiration><Transition><Days>10</Days><StorageClass>GLACIER</StorageClass></Transition></Rule></LifecycleConfiguration>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_put_bucket_acl() {
        check_serialize(
            serialize,
            "s3",
            "PutBucketAcl",
            r#"{"Bucket": "b", "AccessControlPolicy": {"Grants": [{"Grantee": {"Type": "CanonicalUser", "ID": "abc"}, "Permission": "READ"}], "Owner": {"ID": "abc"}}}"#,
            r#"{"method":"PUT","url_path":"/b?acl","query":[],"headers":[],"body":"<AccessControlPolicy xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><AccessControlList><Grant><Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"CanonicalUser\"><ID>abc</ID></Grantee><Permission>READ</Permission></Grant></AccessControlList><Owner><ID>abc</ID></Owner></AccessControlPolicy>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_s3_put_bucket_policy() {
        check_serialize(
            serialize,
            "s3",
            "PutBucketPolicy",
            r#"{"Bucket": "b", "Policy": "{\"Version\": \"2012-10-17\"}"}"#,
            r#"{"method":"PUT","url_path":"/b?policy","query":[],"headers":[],"body":"{\"Version\": \"2012-10-17\"}","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_route53_change_rrsets() {
        check_serialize(
            serialize,
            "route53",
            "ChangeResourceRecordSets",
            r#"{"HostedZoneId": "Z123", "ChangeBatch": {"Comment": "c", "Changes": [{"Action": "UPSERT", "ResourceRecordSet": {"Name": "a.example.com.", "Type": "A", "TTL": 300, "ResourceRecords": [{"Value": "1.2.3.4"}]}}]}}"#,
            r#"{"method":"POST","url_path":"/2013-04-01/hostedzone/Z123/rrset/","query":[],"headers":[],"body":"<ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\"><ChangeBatch><Comment>c</Comment><Changes><Change><Action>UPSERT</Action><ResourceRecordSet><Name>a.example.com.</Name><Type>A</Type><TTL>300</TTL><ResourceRecords><ResourceRecord><Value>1.2.3.4</Value></ResourceRecord></ResourceRecords></ResourceRecordSet></Change></Changes></ChangeBatch></ChangeResourceRecordSetsRequest>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_route53_create_hosted_zone() {
        check_serialize(
            serialize,
            "route53",
            "CreateHostedZone",
            r#"{"Name": "example.com", "CallerReference": "ref-1", "HostedZoneConfig": {"Comment": "x", "PrivateZone": false}}"#,
            r#"{"method":"POST","url_path":"/2013-04-01/hostedzone","query":[],"headers":[],"body":"<CreateHostedZoneRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\"><Name>example.com</Name><CallerReference>ref-1</CallerReference><HostedZoneConfig><Comment>x</Comment><PrivateZone>false</PrivateZone></HostedZoneConfig></CreateHostedZoneRequest>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_route53_list_rrsets() {
        check_serialize(
            serialize,
            "route53",
            "ListResourceRecordSets",
            r#"{"HostedZoneId": "Z1", "StartRecordName": "a b", "MaxItems": "10"}"#,
            r#"{"method":"GET","url_path":"/2013-04-01/hostedzone/Z1/rrset","query":[["name","a b"],["maxitems","10"]],"headers":[],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_cloudfront_create_invalidation() {
        check_serialize(
            serialize,
            "cloudfront",
            "CreateInvalidation",
            r#"{"DistributionId": "E1", "InvalidationBatch": {"Paths": {"Quantity": 2, "Items": ["/*", "/a b"]}, "CallerReference": "cli-1"}}"#,
            r#"{"method":"POST","url_path":"/2020-05-31/distribution/E1/invalidation","query":[],"headers":[],"body":"<InvalidationBatch xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\"><Paths><Quantity>2</Quantity><Items><Path>/*</Path><Path>/a b</Path></Items></Paths><CallerReference>cli-1</CallerReference></InvalidationBatch>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_cloudfront_list_distributions() {
        check_serialize(
            serialize,
            "cloudfront",
            "ListDistributions",
            r#"{"MaxItems": "5"}"#,
            r#"{"method":"GET","url_path":"/2020-05-31/distribution","query":[["MaxItems","5"]],"headers":[],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_cloudfront_update_distribution_header() {
        check_serialize(
            serialize,
            "cloudfront",
            "DeleteDistribution",
            r#"{"Id": "E1", "IfMatch": "ETAG"}"#,
            r#"{"method":"DELETE","url_path":"/2020-05-31/distribution/E1","query":[],"headers":[["If-Match","ETAG"]],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    #[ignore = "needs protocol::timestamp"]
    fn golden_ser_s3_get_object_ts() {
        check_serialize(
            serialize,
            "s3",
            "GetObject",
            r#"{"Bucket": "b", "Key": "k", "IfModifiedSince": "2026-01-02T03:04:05.123Z", "ResponseExpires": "2026-01-02T03:04:05Z"}"#,
            r#"{"method":"GET","url_path":"/b/k","query":[["response-expires","Fri, 02 Jan 2026 03:04:05 GMT"]],"headers":[["If-Modified-Since","Fri, 02 Jan 2026 03:04:05 GMT"]],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    #[ignore = "needs protocol::timestamp"]
    fn golden_ser_s3_put_object_lock_ts() {
        check_serialize(
            serialize,
            "s3",
            "PutObject",
            r#"{"Bucket": "b", "Key": "k", "ObjectLockRetainUntilDate": "2026-01-02T03:04:05.5Z"}"#,
            r#"{"method":"PUT","url_path":"/b/k","query":[],"headers":[["x-amz-object-lock-retain-until-date","2026-01-02T03:04:05Z"]],"body":"","host_prefix":null}"#,
        );
    }

    #[test]
    #[ignore = "needs protocol::timestamp"]
    fn golden_ser_s3_put_object_retention_ts() {
        check_serialize(
            serialize,
            "s3",
            "PutObjectRetention",
            r#"{"Bucket": "b", "Key": "k", "Retention": {"Mode": "GOVERNANCE", "RetainUntilDate": "2026-01-02T03:04:05.25Z"}}"#,
            r#"{"method":"PUT","url_path":"/b/k?retention","query":[],"headers":[],"body":"<Retention xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Mode>GOVERNANCE</Mode><RetainUntilDate>2026-01-02T03:04:05.250000Z</RetainUntilDate></Retention>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_route53_empty_list() {
        check_serialize(
            serialize,
            "route53",
            "ChangeResourceRecordSets",
            r#"{"HostedZoneId": "Z", "ChangeBatch": {"Changes": []}}"#,
            r#"{"method":"POST","url_path":"/2013-04-01/hostedzone/Z/rrset/","query":[],"headers":[],"body":"<ChangeResourceRecordSetsRequest xmlns=\"https://route53.amazonaws.com/doc/2013-04-01/\"><ChangeBatch><Changes /></ChangeBatch></ChangeResourceRecordSetsRequest>","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_parse_s3_list_objects_v2_prefixes() {
        check_parse(
            parse,
            "s3",
            "ListObjectsV2",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>a/</Prefix><KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys><Delimiter>/</Delimiter><IsTruncated>false</IsTruncated><CommonPrefixes><Prefix>a/x/</Prefix></CommonPrefixes><CommonPrefixes><Prefix>a/y/</Prefix></CommonPrefixes></ListBucketResult>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"},"IsTruncated":false,"Name":"s:b","Prefix":"s:a/","Delimiter":"s:/","MaxKeys":"i:1000","CommonPrefixes":[{"Prefix":"s:a/x/"},{"Prefix":"s:a/y/"}],"KeyCount":"i:2"}"#,
        );
    }

    #[test]
    #[ignore = "needs protocol::timestamp"]
    fn golden_parse_s3_list_objects_v2_ts() {
        check_parse(
            parse,
            "s3",
            "ListObjectsV2",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Contents><Key>a.txt</Key><LastModified>2026-01-02T03:04:05.000Z</LastModified><ETag>&quot;abc&quot;</ETag><ChecksumAlgorithm>CRC32</ChecksumAlgorithm><Size>12</Size><StorageClass>STANDARD</StorageClass></Contents><KeyCount>1</KeyCount><IsTruncated>true</IsTruncated><NextContinuationToken>tok</NextContinuationToken></ListBucketResult>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"},"IsTruncated":true,"Contents":[{"Key":"s:a.txt","LastModified":"s:2026-01-02T03:04:05+00:00","ETag":"s:\"abc\"","ChecksumAlgorithm":["s:CRC32"],"Size":"i:12","StorageClass":"s:STANDARD"}],"Name":"s:b","KeyCount":"i:1","NextContinuationToken":"s:tok"}"#,
        );
    }

    #[test]
    fn golden_parse_s3_get_bucket_tagging() {
        check_parse(
            parse,
            "s3",
            "GetBucketTagging",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<Tagging xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><TagSet><Tag><Key>k</Key><Value>v</Value></Tag><Tag><Key>e</Key><Value/></Tag></TagSet></Tagging>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"},"TagSet":[{"Key":"s:k","Value":"s:v"},{"Key":"s:e","Value":"s:"}]}"#,
        );
    }

    #[test]
    fn golden_parse_s3_get_bucket_versioning_empty() {
        check_parse(
            parse,
            "s3",
            "GetBucketVersioning",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_get_bucket_acl() {
        check_parse(
            parse,
            "s3",
            "GetBucketAcl",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Owner><ID>abc</ID></Owner><AccessControlList><Grant><Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser"><ID>abc</ID><DisplayName>me</DisplayName></Grantee><Permission>FULL_CONTROL</Permission></Grant><Grant><Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group"><URI>http://acs.amazonaws.com/groups/global/AllUsers</URI></Grantee><Permission>READ</Permission></Grant></AccessControlList></AccessControlPolicy>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"},"Owner":{"ID":"s:abc"},"Grants":[{"Grantee":{"DisplayName":"s:me","ID":"s:abc","Type":"s:CanonicalUser"},"Permission":"s:FULL_CONTROL"},{"Grantee":{"Type":"s:Group","URI":"s:http://acs.amazonaws.com/groups/global/AllUsers"},"Permission":"s:READ"}]}"#,
        );
    }

    #[test]
    fn golden_parse_s3_head_object() {
        check_parse(
            parse,
            "s3",
            "HeadObject",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"], ["Content-Length", "12"], ["ETag", "\"abc\""], ["x-amz-meta-Foo", "bar"], ["x-amz-meta-baz", "q"], ["accept-ranges", "bytes"], ["x-amz-missing-meta", "2"], ["x-amz-server-side-encryption-bucket-key-enabled", "true"], ["x-amz-meta-baz", "r"]]"#,
            r#""#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3","content-length":"s:12","etag":"s:\"abc\"","x-amz-meta-foo":"s:bar","x-amz-meta-baz":"s:q, r","accept-ranges":"s:bytes","x-amz-missing-meta":"s:2","x-amz-server-side-encryption-bucket-key-enabled":"s:true"},"RetryAttempts":"i:0"},"AcceptRanges":"s:bytes","ContentLength":"i:12","ETag":"s:\"abc\"","MissingMeta":"i:2","ContentType":"s:application/xml","Metadata":{"Foo":"s:bar","baz":"s:q, r"},"BucketKeyEnabled":true}"#,
        );
    }

    #[test]
    #[ignore = "needs protocol::timestamp"]
    fn golden_parse_s3_head_object_ts() {
        check_parse(
            parse,
            "s3",
            "HeadObject",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"], ["Last-Modified", "Tue, 03 Nov 2026 00:00:00 GMT"], ["Content-Length", "0"]]"#,
            r#""#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3","last-modified":"s:Tue, 03 Nov 2026 00:00:00 GMT","content-length":"s:0"},"RetryAttempts":"i:0"},"LastModified":"s:2026-11-03T00:00:00+00:00","ContentLength":"i:0","ContentType":"s:application/xml","Metadata":{}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_get_object_stream() {
        check_parse(
            parse,
            "s3",
            "GetObject",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"], ["Content-Length", "5"], ["Content-Type", "text/plain"], ["Content-Range", "bytes 0-4/10"]]"#,
            r#"hello"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml, text/plain","server":"s:AmazonS3","content-length":"s:5","content-range":"s:bytes 0-4/10"},"RetryAttempts":"i:0"},"ContentLength":"i:5","ContentRange":"s:bytes 0-4/10","ContentType":"s:application/xml, text/plain","Metadata":{},"Body":"b:68656c6c6f"}"#,
        );
    }

    #[test]
    fn golden_parse_s3_get_bucket_policy() {
        check_parse(
            parse,
            "s3",
            "GetBucketPolicy",
            200,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"{"Version":"2012-10-17"}"#,
            r#"{"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"},"Policy":"s:{\"Version\":\"2012-10-17\"}"}"#,
        );
    }

    #[test]
    fn golden_parse_s3_error_no_such_key() {
        check_parse_error(
            parse_error,
            "s3",
            "GetObject",
            404,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message><Key>k</Key><RequestId>REQ</RequestId><HostId>host-id</HostId></Error>"#,
            r#"{"Error":{"Code":"s:NoSuchKey","Message":"s:The specified key does not exist.","Key":"s:k"},"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_error_no_such_bucket() {
        check_parse_error(
            parse_error,
            "s3",
            "ListObjectsV2",
            404,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message><BucketName>b</BucketName><RequestId>REQ</RequestId><HostId>host-id</HostId></Error>"#,
            r#"{"Error":{"Code":"s:NoSuchBucket","Message":"s:The specified bucket does not exist","BucketName":"s:b"},"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_error_head_empty() {
        check_parse_error(
            parse_error,
            "s3",
            "HeadObject",
            404,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#""#,
            r#"{"Error":{"Code":"s:404","Message":"s:Not Found"},"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_error_not_xml() {
        check_parse_error(
            parse_error,
            "s3",
            "GetObject",
            403,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#"Forbidden"#,
            r#"{"Error":{"Code":"s:403","Message":"s:Forbidden"},"ResponseMetadata":{"RequestId":"s:REQ","HostId":"s:host-id","HTTPStatusCode":"i:403","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_s3_error_generic_5xx() {
        check_parse_error(
            parse_error,
            "s3",
            "GetObject",
            503,
            r#"[["x-amz-id-2", "host-id"], ["x-amz-request-id", "REQ"], ["Content-Type", "application/xml"], ["Server", "AmazonS3"]]"#,
            r#""#,
            r#"{"Error":{"Code":"s:503","Message":"s:Service Unavailable"},"ResponseMetadata":{"HTTPStatusCode":"i:503","HTTPHeaders":{"x-amz-id-2":"s:host-id","x-amz-request-id":"s:REQ","content-type":"s:application/xml","server":"s:AmazonS3"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_route53_get_hosted_zone() {
        check_parse(
            parse,
            "route53",
            "GetHostedZone",
            200,
            r#"[["x-amzn-RequestId", "r53"], ["Content-Type", "text/xml"]]"#,
            r#"<?xml version="1.0"?>
<GetHostedZoneResponse xmlns="https://route53.amazonaws.com/doc/2013-04-01/"><HostedZone><Id>/hostedzone/Z1</Id><Name>example.com.</Name><CallerReference>ref</CallerReference><Config><Comment>c</Comment><PrivateZone>false</PrivateZone></Config><ResourceRecordSetCount>2</ResourceRecordSetCount></HostedZone><DelegationSet><NameServers><NameServer>ns-1.awsdns-00.com</NameServer><NameServer>ns-2.awsdns-00.net</NameServer></NameServers></DelegationSet></GetHostedZoneResponse>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:r53","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:r53","content-type":"s:text/xml"},"RetryAttempts":"i:0"},"HostedZone":{"Id":"s:/hostedzone/Z1","Name":"s:example.com.","CallerReference":"s:ref","Config":{"Comment":"s:c","PrivateZone":false},"ResourceRecordSetCount":"i:2"},"DelegationSet":{"NameServers":["s:ns-1.awsdns-00.com","s:ns-2.awsdns-00.net"]}}"#,
        );
    }

    #[test]
    fn golden_parse_route53_list_rrsets() {
        check_parse(
            parse,
            "route53",
            "ListResourceRecordSets",
            200,
            r#"[["x-amzn-RequestId", "r"]]"#,
            r#"<ListResourceRecordSetsResponse xmlns="https://route53.amazonaws.com/doc/2013-04-01/"><ResourceRecordSets><ResourceRecordSet><Name>a.example.com.</Name><Type>A</Type><TTL>300</TTL><ResourceRecords><ResourceRecord><Value>1.2.3.4</Value></ResourceRecord></ResourceRecords></ResourceRecordSet></ResourceRecordSets><IsTruncated>false</IsTruncated><MaxItems>100</MaxItems></ListResourceRecordSetsResponse>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:r"},"RetryAttempts":"i:0"},"ResourceRecordSets":[{"Name":"s:a.example.com.","Type":"s:A","TTL":"i:300","ResourceRecords":[{"Value":"s:1.2.3.4"}]}],"IsTruncated":false,"MaxItems":"s:100"}"#,
        );
    }

    #[test]
    fn golden_parse_route53_error_modeled() {
        check_parse_error(
            parse_error,
            "route53",
            "GetHostedZone",
            404,
            r#"[["x-amzn-RequestId", "r53e"], ["Content-Type", "text/xml"]]"#,
            r#"<?xml version="1.0"?>
<ErrorResponse xmlns="https://route53.amazonaws.com/doc/2013-04-01/"><Error><Type>Sender</Type><Code>NoSuchHostedZone</Code><Message>No hosted zone found with ID: Z1</Message></Error><RequestId>r53e</RequestId></ErrorResponse>"#,
            r#"{"Error":{"Message":"s:No hosted zone found with ID: Z1","Code":"s:NoSuchHostedZone","Type":"s:Sender"},"ResponseMetadata":{"RequestId":"s:r53e","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amzn-requestid":"s:r53e","content-type":"s:text/xml"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_cloudfront_list_distributions() {
        check_parse(
            parse,
            "cloudfront",
            "ListDistributions",
            200,
            r#"[["x-amzn-RequestId", "cf"]]"#,
            r#"<DistributionList xmlns="http://cloudfront.amazonaws.com/doc/2020-05-31/"><Marker></Marker><MaxItems>100</MaxItems><IsTruncated>false</IsTruncated><Quantity>0</Quantity></DistributionList>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:cf","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:cf"},"RetryAttempts":"i:0"},"DistributionList":{"Marker":"s:","MaxItems":"i:100","IsTruncated":false,"Quantity":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_cloudfront_get_distribution_config_etag() {
        check_parse(
            parse,
            "cloudfront",
            "GetCachePolicyConfig",
            200,
            r#"[["x-amzn-RequestId", "cf"], ["ETag", "E2"]]"#,
            r#"<CachePolicyConfig xmlns="http://cloudfront.amazonaws.com/doc/2020-05-31/"><Comment>c</Comment><Name>n</Name><DefaultTTL>86400</DefaultTTL><MaxTTL>31536000</MaxTTL><MinTTL>1</MinTTL></CachePolicyConfig>"#,
            r#"{"ResponseMetadata":{"RequestId":"s:cf","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:cf","etag":"s:E2"},"RetryAttempts":"i:0"},"ETag":"s:E2","CachePolicyConfig":{"Comment":"s:c","Name":"s:n","DefaultTTL":"i:86400","MaxTTL":"i:31536000","MinTTL":"i:1"}}"#,
        );
    }

    #[test]
    fn golden_parse_cloudfront_error() {
        check_parse_error(
            parse_error,
            "cloudfront",
            "GetDistribution",
            404,
            r#"[["x-amzn-RequestId", "cf"]]"#,
            r#"<?xml version="1.0"?><ErrorResponse xmlns="http://cloudfront.amazonaws.com/doc/2020-05-31/"><Error><Type>Sender</Type><Code>NoSuchDistribution</Code><Message>The specified distribution does not exist.</Message></Error><RequestId>cf</RequestId></ErrorResponse>"#,
            r#"{"Error":{"Message":"s:The specified distribution does not exist.","Code":"s:NoSuchDistribution","Type":"s:Sender"},"ResponseMetadata":{"RequestId":"s:cf","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amzn-requestid":"s:cf"},"RetryAttempts":"i:0"},"Message":"s:The specified distribution does not exist."}"#,
        );
    }
}
