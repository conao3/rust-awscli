//! The `query` protocol: `QuerySerializer` (`botocore/serialize.py`) and
//! `QueryParser` (`botocore/parsers.py`).

use anyhow::{Result, anyhow};
use indexmap::IndexMap;

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::xml_common::{
    self, Flavor, XmlParser, b64encode, expand_host_prefix, float_str, py_str, quote_plus,
    serialized_name, timestamp_str,
};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::Value;

/// The serializer variant: `QuerySerializer` or `EC2Serializer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Query,
    Ec2,
}

struct QuerySerializer {
    variant: Variant,
}

impl QuerySerializer {
    /// `_get_serialized_name` of `QuerySerializer` / `EC2Serializer`.
    fn serialized_name<'a>(&self, shape: &Shape<'a>, default: &'a str) -> String {
        match self.variant {
            Variant::Query => serialized_name(shape).unwrap_or(default).to_string(),
            Variant::Ec2 => {
                if let Some(q) = shape.get_str("queryName") {
                    q.to_string()
                } else if let Some(name) = serialized_name(shape) {
                    let mut chars = name.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                        None => String::new(),
                    }
                } else {
                    default.to_string()
                }
            }
        }
    }

    fn serialize(
        &self,
        out: &mut IndexMap<String, String>,
        value: &Value,
        shape: &Shape<'_>,
        prefix: &str,
    ) -> Result<()> {
        match shape.type_name() {
            "structure" => {
                let Some(map) = value.as_map() else {
                    return Err(anyhow!("expected a structure for {}", shape.name));
                };
                for (key, v) in map {
                    let member = shape
                        .member_shape(key)
                        .ok_or_else(|| anyhow!("unknown parameter: {key}"))?;
                    let mut member_prefix = self.serialized_name(&member, key);
                    if !prefix.is_empty() {
                        member_prefix = format!("{prefix}.{member_prefix}");
                    }
                    self.serialize(out, v, &member, &member_prefix)?;
                }
            }
            "list" => {
                let items = value.as_list().cloned().unwrap_or_default();
                let member = shape
                    .list_member()
                    .ok_or_else(|| anyhow!("list {} without member", shape.name))?;
                let list_prefix = match self.variant {
                    Variant::Ec2 => prefix.to_string(),
                    Variant::Query => {
                        if items.is_empty() {
                            out.insert(prefix.to_string(), String::new());
                            return Ok(());
                        }
                        if shape.get_bool("flattened") {
                            match serialized_name(&member) {
                                Some(_) => {
                                    let name = self.serialized_name(&member, "");
                                    let mut parts: Vec<&str> = prefix.split('.').collect();
                                    parts.pop();
                                    parts.push(&name);
                                    parts.join(".")
                                }
                                None => prefix.to_string(),
                            }
                        } else {
                            let list_name = serialized_name(&member).unwrap_or("member");
                            format!("{prefix}.{list_name}")
                        }
                    }
                };
                for (i, element) in items.iter().enumerate() {
                    let element_prefix = format!("{list_prefix}.{}", i + 1);
                    self.serialize(out, element, &member, &element_prefix)?;
                }
            }
            "map" => {
                let full_prefix = if shape.get_bool("flattened") {
                    prefix.to_string()
                } else {
                    format!("{prefix}.entry")
                };
                let key_shape = shape.map_key().ok_or_else(|| anyhow!("map without key"))?;
                let value_shape = shape
                    .map_value()
                    .ok_or_else(|| anyhow!("map without value"))?;
                let key_suffix = self.serialized_name(&key_shape, "key");
                let value_suffix = self.serialized_name(&value_shape, "value");
                if let Some(map) = value.as_map() {
                    for (i, (k, v)) in map.iter().enumerate() {
                        let n = i + 1;
                        let key_prefix = format!("{full_prefix}.{n}.{key_suffix}");
                        let value_prefix = format!("{full_prefix}.{n}.{value_suffix}");
                        self.serialize(out, &Value::Str(k.clone()), &key_shape, &key_prefix)?;
                        self.serialize(out, v, &value_shape, &value_prefix)?;
                    }
                }
            }
            "blob" => {
                out.insert(prefix.to_string(), b64encode(value));
            }
            "timestamp" => {
                let s = timestamp_str(value, shape.get_str("timestampFormat"))?;
                out.insert(prefix.to_string(), s);
            }
            "boolean" => {
                let s = if value.is_truthy() { "true" } else { "false" };
                out.insert(prefix.to_string(), s.to_string());
            }
            "float" | "double" => {
                out.insert(prefix.to_string(), float_str(value));
            }
            _ => {
                out.insert(prefix.to_string(), py_str(value));
            }
        }
        Ok(())
    }
}

/// `QuerySerializer.serialize_to_request` / `EC2Serializer`, with the body
/// dict encoded as `AWSRequestPreparer._prepare_body` does
/// (`urlencode(..., doseq=True)`).
pub(crate) fn serialize_variant(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
    variant: Variant,
) -> Result<HttpRequest> {
    let mut body: IndexMap<String, String> = IndexMap::new();
    body.insert("Action".into(), operation.name.to_string());
    body.insert(
        "Version".into(),
        model
            .metadata_str("apiVersion")
            .unwrap_or_default()
            .to_string(),
    );
    if let Some(shape) = operation.input() {
        QuerySerializer { variant }.serialize(&mut body, params, &shape, "")?;
    }
    let encoded: Vec<String> = body
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k.as_bytes()), quote_plus(v.as_bytes())))
        .collect();
    Ok(HttpRequest {
        method: operation.http_method().to_string(),
        url_path: "/".into(),
        query: Vec::new(),
        headers: vec![(
            "Content-Type".into(),
            "application/x-www-form-urlencoded; charset=utf-8".into(),
        )],
        body: encoded.join("&").into_bytes(),
        host_prefix: expand_host_prefix(operation, params)?,
    })
}

/// `QuerySerializer.serialize_to_request`.
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    serialize_variant(model, operation, params, Variant::Query)
}

/// `QueryParser.parse` (plus the endpoint's modeled error fields and
/// `RetryAttempts`).
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let parser = XmlParser {
        flavor: Flavor::Query,
        model,
        options: *options,
    };
    parser.parse(operation.output(), response, false)
}

/// `QueryParser` error parsing (`_do_error_parse` and the modeled error
/// fields).
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let parser = XmlParser {
        flavor: Flavor::Query,
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
    fn golden_ser_sts_get_caller_identity() {
        check_serialize(
            serialize,
            "sts",
            "GetCallerIdentity",
            r#"{}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=GetCallerIdentity&Version=2011-06-15","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_sts_assume_role() {
        check_serialize(
            serialize,
            "sts",
            "AssumeRole",
            r#"{"RoleArn": "arn:aws:iam::123456789012:role/demo", "RoleSessionName": "my session", "DurationSeconds": 3600, "Tags": [{"Key": "k1", "Value": "v 1"}, {"Key": "k2", "Value": "ü&="}], "PolicyArns": [{"arn": "arn:aws:iam::aws:policy/ReadOnlyAccess"}], "TransitiveTagKeys": []}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=AssumeRole&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fdemo&RoleSessionName=my+session&DurationSeconds=3600&Tags.member.1.Key=k1&Tags.member.1.Value=v+1&Tags.member.2.Key=k2&Tags.member.2.Value=%C3%BC%26%3D&PolicyArns.member.1.arn=arn%3Aaws%3Aiam%3A%3Aaws%3Apolicy%2FReadOnlyAccess&TransitiveTagKeys=","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_iam_put_user_policy() {
        check_serialize(
            serialize,
            "iam",
            "PutUserPolicy",
            r#"{"UserName": "bob", "PolicyName": "p", "PolicyDocument": "{\"Version\": \"2012-10-17\", \"Statement\": [{\"Effect\": \"Allow\", \"Action\": \"s3:*\", \"Resource\": \"*\"}]}"}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=PutUserPolicy&Version=2010-05-08&UserName=bob&PolicyName=p&PolicyDocument=%7B%22Version%22%3A+%222012-10-17%22%2C+%22Statement%22%3A+%5B%7B%22Effect%22%3A+%22Allow%22%2C+%22Action%22%3A+%22s3%3A%2A%22%2C+%22Resource%22%3A+%22%2A%22%7D%5D%7D","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_iam_create_user() {
        check_serialize(
            serialize,
            "iam",
            "CreateUser",
            r#"{"UserName": "bob", "Path": "/a/b/", "Tags": [{"Key": "team", "Value": "x~y*z"}]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=CreateUser&Version=2010-05-08&UserName=bob&Path=%2Fa%2Fb%2F&Tags.member.1.Key=team&Tags.member.1.Value=x~y%2Az","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_sns_publish() {
        check_serialize(
            serialize,
            "sns",
            "Publish",
            r#"{"TopicArn": "arn:aws:sns:us-east-1:123456789012:t", "Message": "hello world\nline2", "MessageAttributes": {"a1": {"DataType": "String", "StringValue": "v"}, "a2": {"DataType": "Binary", "BinaryValue": "raw bytes"}}}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=Publish&Version=2010-03-31&TopicArn=arn%3Aaws%3Asns%3Aus-east-1%3A123456789012%3At&Message=hello+world%0Aline2&MessageAttributes.entry.1.Name=a1&MessageAttributes.entry.1.Value.DataType=String&MessageAttributes.entry.1.Value.StringValue=v&MessageAttributes.entry.2.Name=a2&MessageAttributes.entry.2.Value.DataType=Binary&MessageAttributes.entry.2.Value.BinaryValue=cmF3IGJ5dGVz","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_cloudformation_create_stack() {
        check_serialize(
            serialize,
            "cloudformation",
            "CreateStack",
            r#"{"StackName": "s", "TemplateBody": "{\"Resources\": {}}", "Parameters": [{"ParameterKey": "Env", "ParameterValue": "prod"}], "Capabilities": ["CAPABILITY_IAM", "CAPABILITY_NAMED_IAM"], "DisableRollback": true, "TimeoutInMinutes": 5, "Tags": [{"Key": "a", "Value": "b"}]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=CreateStack&Version=2010-05-15&StackName=s&TemplateBody=%7B%22Resources%22%3A+%7B%7D%7D&Parameters.member.1.ParameterKey=Env&Parameters.member.1.ParameterValue=prod&Capabilities.member.1=CAPABILITY_IAM&Capabilities.member.2=CAPABILITY_NAMED_IAM&DisableRollback=true&TimeoutInMinutes=5&Tags.member.1.Key=a&Tags.member.1.Value=b","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_elb_create_load_balancer() {
        check_serialize(
            serialize,
            "elb",
            "CreateLoadBalancer",
            r#"{"LoadBalancerName": "lb", "Listeners": [{"Protocol": "HTTP", "LoadBalancerPort": 80, "InstanceProtocol": "HTTP", "InstancePort": 8080}], "AvailabilityZones": ["us-east-1a", "us-east-1b"]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=CreateLoadBalancer&Version=2012-06-01&LoadBalancerName=lb&Listeners.member.1.Protocol=HTTP&Listeners.member.1.LoadBalancerPort=80&Listeners.member.1.InstanceProtocol=HTTP&Listeners.member.1.InstancePort=8080&AvailabilityZones.member.1=us-east-1a&AvailabilityZones.member.2=us-east-1b","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_rds_modify_db_cluster() {
        check_serialize(
            serialize,
            "rds",
            "ModifyDBCluster",
            r#"{"DBClusterIdentifier": "c1", "ApplyImmediately": false, "ServerlessV2ScalingConfiguration": {"MinCapacity": 0.5, "MaxCapacity": 1e+16}}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=ModifyDBCluster&Version=2014-10-31&DBClusterIdentifier=c1&ApplyImmediately=false&ServerlessV2ScalingConfiguration.MinCapacity=0.5&ServerlessV2ScalingConfiguration.MaxCapacity=1e%2B16","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_autoscaling_create_group() {
        check_serialize(
            serialize,
            "autoscaling",
            "CreateAutoScalingGroup",
            r#"{"AutoScalingGroupName": "g", "MinSize": 0, "MaxSize": 2, "Tags": [{"Key": "Name", "Value": "n", "PropagateAtLaunch": false}], "AvailabilityZones": ["us-east-1a"]}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=CreateAutoScalingGroup&Version=2011-01-01&AutoScalingGroupName=g&MinSize=0&MaxSize=2&Tags.member.1.Key=Name&Tags.member.1.Value=n&Tags.member.1.PropagateAtLaunch=false&AvailabilityZones.member.1=us-east-1a","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_sdb_put_attributes_flattened() {
        check_serialize(
            serialize,
            "sdb",
            "PutAttributes",
            r#"{"DomainName": "d", "ItemName": "i", "Attributes": [{"Name": "a", "Value": "1", "Replace": true}, {"Name": "b", "Value": "2"}], "Expected": {"Name": "a", "Exists": false}}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=PutAttributes&Version=2009-04-15&DomainName=d&ItemName=i&Attribute.1.Name=a&Attribute.1.Value=1&Attribute.1.Replace=true&Attribute.2.Name=b&Attribute.2.Value=2&Expected.Name=a&Expected.Exists=false","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_sts_assume_role_policy_float() {
        check_serialize(
            serialize,
            "sts",
            "AssumeRole",
            r#"{"RoleArn": "r", "RoleSessionName": "s", "Policy": "{}"}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=AssumeRole&Version=2011-06-15&RoleArn=r&RoleSessionName=s&Policy=%7B%7D","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_cloudformation_list_stacks_ts() {
        check_serialize(
            serialize,
            "cloudformation",
            "DescribeStackEvents",
            r#"{"StackName": "s"}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=DescribeStackEvents&Version=2010-05-15&StackName=s","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_ser_rds_describe_events_ts() {
        check_serialize(
            serialize,
            "rds",
            "DescribeEvents",
            r#"{"StartTime": "2024-01-02T03:04:05Z", "Duration": 60}"#,
            r#"{"method":"POST","url_path":"/","query":[],"headers":[["Content-Type","application/x-www-form-urlencoded; charset=utf-8"]],"body":"Action=DescribeEvents&Version=2014-10-31&StartTime=2024-01-02T03%3A04%3A05Z&Duration=60","host_prefix":null}"#,
        );
    }

    #[test]
    fn golden_parse_sts_get_caller_identity() {
        check_parse(
            parse,
            "sts",
            "GetCallerIdentity",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetCallerIdentityResult>
    <Arn>arn:aws:iam::123456789012:user/bob</Arn>
    <UserId>AIDAEXAMPLE</UserId>
    <Account>123456789012</Account>
  </GetCallerIdentityResult>
  <ResponseMetadata>
    <RequestId>req-1</RequestId>
  </ResponseMetadata>
</GetCallerIdentityResponse>
"#,
            r#"{"UserId":"s:AIDAEXAMPLE","Account":"s:123456789012","Arn":"s:arn:aws:iam::123456789012:user/bob","ResponseMetadata":{"RequestId":"s:req-1","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sts_assume_role_ts() {
        check_parse(
            parse,
            "sts",
            "AssumeRole",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleResult><Credentials><AccessKeyId>AKIA</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken><Expiration>2026-11-03T01:00:00Z</Expiration></Credentials><AssumedRoleUser><AssumedRoleId>AROA:s</AssumedRoleId><Arn>arn</Arn></AssumedRoleUser><PackedPolicySize>6</PackedPolicySize></AssumeRoleResult><ResponseMetadata><RequestId>req-2</RequestId></ResponseMetadata></AssumeRoleResponse>"#,
            r#"{"Credentials":{"AccessKeyId":"s:AKIA","SecretAccessKey":"s:s","SessionToken":"s:t","Expiration":"s:2026-11-03T01:00:00+00:00"},"AssumedRoleUser":{"AssumedRoleId":"s:AROA:s","Arn":"s:arn"},"PackedPolicySize":"i:6","ResponseMetadata":{"RequestId":"s:req-2","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_iam_list_user_tags() {
        check_parse(
            parse,
            "iam",
            "ListUserTags",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ListUserTagsResponse xmlns="https://iam.amazonaws.com/doc/2010-05-08/"><ListUserTagsResult><IsTruncated>false</IsTruncated><Tags><member><Key>a</Key><Value>1 &amp; 2</Value></member><member><Key>b</Key><Value></Value></member></Tags></ListUserTagsResult><ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></ListUserTagsResponse>"#,
            r#"{"Tags":[{"Key":"s:a","Value":"s:1 & 2"},{"Key":"s:b","Value":"s:"}],"IsTruncated":false,"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_iam_list_users_ts() {
        check_parse(
            parse,
            "iam",
            "ListUsers",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ListUsersResponse><ListUsersResult><Users><member><UserName>bob</UserName><Path>/</Path><UserId>U</UserId><Arn>arn</Arn><CreateDate>2020-01-01T00:00:00Z</CreateDate></member></Users><IsTruncated>true</IsTruncated><Marker>m</Marker></ListUsersResult></ListUsersResponse>"#,
            r#"{"Users":[{"Path":"s:/","UserName":"s:bob","UserId":"s:U","Arn":"s:arn","CreateDate":"s:2020-01-01T00:00:00+00:00"}],"IsTruncated":true,"Marker":"s:m","ResponseMetadata":{"HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sns_get_topic_attributes() {
        check_parse(
            parse,
            "sns",
            "GetTopicAttributes",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<GetTopicAttributesResponse xmlns="http://sns.amazonaws.com/doc/2010-03-31/"><GetTopicAttributesResult><Attributes><entry><key>Owner</key><value>123</value></entry><entry><key>Policy</key><value>{"a":1}</value></entry></Attributes></GetTopicAttributesResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></GetTopicAttributesResponse>"#,
            r#"{"Attributes":{"Owner":"s:123","Policy":"s:{\"a\":1}"},"ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sns_list_topics_empty() {
        check_parse(
            parse,
            "sns",
            "ListTopics",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ListTopicsResponse><ListTopicsResult><Topics/></ListTopicsResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></ListTopicsResponse>"#,
            r#"{"Topics":[],"ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_cloudformation_list_exports() {
        check_parse(
            parse,
            "cloudformation",
            "ListExports",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ListExportsResponse><ListExportsResult><Exports><member><ExportingStackId>id</ExportingStackId><Name>n</Name><Value>v</Value></member></Exports><NextToken>tok</NextToken></ListExportsResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></ListExportsResponse>"#,
            r#"{"Exports":[{"ExportingStackId":"s:id","Name":"s:n","Value":"s:v"}],"NextToken":"s:tok","ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_elb_describe_tags() {
        check_parse(
            parse,
            "elb",
            "DescribeTags",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<DescribeTagsResponse><DescribeTagsResult><TagDescriptions><member><LoadBalancerName>lb</LoadBalancerName><Tags><member><Key>k</Key><Value>v</Value></member></Tags></member></TagDescriptions></DescribeTagsResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></DescribeTagsResponse>"#,
            r#"{"TagDescriptions":[{"LoadBalancerName":"s:lb","Tags":[{"Key":"s:k","Value":"s:v"}]}],"ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_rds_describe_account_attributes() {
        check_parse(
            parse,
            "rds",
            "DescribeAccountAttributes",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<DescribeAccountAttributesResponse><DescribeAccountAttributesResult><AccountQuotas><AccountQuota><AccountQuotaName>DBInstances</AccountQuotaName><Used>3</Used><Max>40</Max></AccountQuota></AccountQuotas></DescribeAccountAttributesResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></DescribeAccountAttributesResponse>"#,
            r#"{"AccountQuotas":[{"AccountQuotaName":"s:DBInstances","Used":"i:3","Max":"i:40"}],"ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_autoscaling_describe_tags() {
        check_parse(
            parse,
            "autoscaling",
            "DescribeTags",
            200,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<DescribeTagsResponse><DescribeTagsResult><Tags><member><ResourceId>g</ResourceId><ResourceType>auto-scaling-group</ResourceType><Key>k</Key><Value>v</Value><PropagateAtLaunch>true</PropagateAtLaunch></member></Tags></DescribeTagsResult><ResponseMetadata><RequestId>x</RequestId></ResponseMetadata></DescribeTagsResponse>"#,
            r#"{"Tags":[{"ResourceId":"s:g","ResourceType":"s:auto-scaling-group","Key":"s:k","Value":"s:v","PropagateAtLaunch":true}],"ResponseMetadata":{"RequestId":"s:x","HTTPStatusCode":"i:200","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sts_error_access_denied() {
        check_parse_error(
            parse_error,
            "sts",
            "AssumeRole",
            403,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <Error>
    <Type>Sender</Type>
    <Code>AccessDenied</Code>
    <Message>User is not authorized</Message>
  </Error>
  <RequestId>req-err</RequestId>
</ErrorResponse>
"#,
            r#"{"Error":{"Type":"s:Sender","Code":"s:AccessDenied","Message":"s:User is not authorized"},"ResponseMetadata":{"RequestId":"s:req-err","HTTPStatusCode":"i:403","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_iam_error_modeled() {
        check_parse_error(
            parse_error,
            "iam",
            "GetUser",
            404,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ErrorResponse><Error><Type>Sender</Type><Code>NoSuchEntity</Code><Message>The user with name x cannot be found.</Message></Error><RequestId>r</RequestId></ErrorResponse>"#,
            r#"{"Error":{"Type":"s:Sender","Code":"s:NoSuchEntity","Message":"s:The user with name x cannot be found."},"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:404","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sts_error_generic_html() {
        check_parse_error(
            parse_error,
            "sts",
            "GetCallerIdentity",
            503,
            r#"[]"#,
            r#"  <html><body>Service Unavailable</body></html>"#,
            r#"{"Error":{"Code":"s:503","Message":"s:Service Unavailable"},"ResponseMetadata":{"HTTPStatusCode":"i:503","HTTPHeaders":{},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sts_error_generic_empty() {
        check_parse_error(
            parse_error,
            "sts",
            "GetCallerIdentity",
            500,
            r#"[["Content-Length", "0"]]"#,
            r#""#,
            r#"{"Error":{"Code":"s:500","Message":"s:Internal Server Error"},"ResponseMetadata":{"HTTPStatusCode":"i:500","HTTPHeaders":{"content-length":"s:0"},"RetryAttempts":"i:0"}}"#,
        );
    }

    #[test]
    fn golden_parse_sns_error_empty_message() {
        check_parse_error(
            parse_error,
            "sns",
            "Publish",
            400,
            r#"[["x-amzn-RequestId", "req-1"], ["Content-Type", "text/xml"], ["Date", "Tue, 03 Nov 2026 00:00:00 GMT"]]"#,
            r#"<ErrorResponse><Error><Type>Sender</Type><Code>NotFound</Code><Message/></Error><RequestId>r</RequestId></ErrorResponse>"#,
            r#"{"Error":{"Type":"s:Sender","Code":"s:NotFound","Message":null},"ResponseMetadata":{"RequestId":"s:r","HTTPStatusCode":"i:400","HTTPHeaders":{"x-amzn-requestid":"s:req-1","content-type":"s:text/xml","date":"s:Tue, 03 Nov 2026 00:00:00 GMT"},"RetryAttempts":"i:0"}}"#,
        );
    }
}
