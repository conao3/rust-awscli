//! Goldens produced by the reference botocore and CLI handlers
//! (`testdata/goldens.py`).

use std::sync::OnceLock;

use chrono::TimeZone;

use super::*;
use crate::session::sign::Credentials;

fn goldens() -> &'static Map {
    static G: OnceLock<Map> = OnceLock::new();
    G.get_or_init(|| {
        include_str!("testdata/goldens.jsonl")
            .lines()
            .map(|line| {
                let entry = Value::parse_json(line).unwrap();
                let name = s(entry.get("name").unwrap()).to_string();
                (name, entry.get("value").unwrap().clone())
            })
            .collect()
    })
}

fn golden(key: &str) -> &'static Value {
    goldens().get(key).unwrap_or_else(|| panic!("golden {key}"))
}

fn json(text: &str) -> Value {
    Value::parse_json(text).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn pairs(v: &Value) -> Vec<(&Value, &Value)> {
    v.as_list()
        .unwrap()
        .iter()
        .map(|p| {
            let l = p.as_list().unwrap();
            (&l[0], &l[1])
        })
        .collect()
}

fn test_body(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + 3) % 251) as u8).collect()
}

#[test]
fn tree_hash() {
    for (n, expected) in pairs(golden("tree_hash")) {
        let n = n.as_i64().unwrap() as usize;
        assert_eq!(
            glacier::calculate_tree_hash(&test_body(n)),
            s(expected),
            "{n}"
        );
    }
}

#[test]
fn sse_md5() {
    let mut params =
        json(r#"{"SSECustomerKey": "/path/to/thing", "CopySourceSSECustomerKey": "ü key"}"#);
    let map = params.as_map_mut().unwrap();
    s3::sse_md5(map, "SSECustomer");
    s3::sse_md5(map, "CopySourceSSECustomer");
    assert_eq!(&params, golden("sse_md5"));
}

#[test]
fn copy_source() {
    for (input, expected) in pairs(golden("copy_source")) {
        assert_eq!(s3::quote_source_header(s(input)), s(expected), "{input:?}");
    }
    let s3_model = ServiceModel::load("s3").unwrap();
    let op = s3_model.operation("CopyObject").unwrap();
    let mut params = json(
        r#"{"Bucket": "b", "Key": "k", "CopySource": {"Bucket": "arn:aws:s3:us-west-2:123456789012:accesspoint/ap", "Key": "k y", "VersionId": "v1"}}"#,
    );
    let mut context = CallContext::default();
    s3::before_parameter_build(&op, params.as_map_mut().unwrap(), &mut context).unwrap();
    assert_eq!(params.get("CopySource"), Some(golden("copy_source_dict")));
}

#[test]
fn unquote_matches_python() {
    for (input, expected) in pairs(golden("unquote")) {
        assert_eq!(unquote(s(input)), s(expected), "{input:?}");
    }
}

fn s3_param_error(op: &str, params: &str) -> Option<String> {
    let model = ServiceModel::load("s3").unwrap();
    let op = model.operation(op).unwrap();
    let mut params = json(params);
    let mut context = CallContext::default();
    s3::before_parameter_build(&op, params.as_map_mut().unwrap(), &mut context)
        .err()
        .map(|e| e.to_string())
}

#[test]
fn validate_bucket_name() {
    assert_eq!(
        s3_param_error("ListObjectsV2", r#"{"Bucket": "bad bucket!"}"#).as_deref(),
        Some(s(golden("bucket_error")))
    );
    for (bucket, ok) in pairs(golden("bucket_ok")) {
        let params = Value::Map(Map::from_iter([("Bucket".to_string(), bucket.clone())]));
        let text = crate::pyfmt::json_dumps_default(&params);
        let result = s3_param_error("HeadBucket", &text);
        assert_eq!(result.is_none(), *ok == Value::Bool(true), "{bucket:?}");
    }
}

#[test]
fn validate_ascii_metadata() {
    assert_eq!(
        s3_param_error(
            "PutObject",
            r#"{"Bucket": "b", "Key": "k", "Metadata": {"k": "vé"}}"#
        )
        .as_deref(),
        Some(s(golden("ascii_error")))
    );
}

#[test]
fn list_objects_encoding_type() {
    let model = ServiceModel::load("s3").unwrap();
    let op = model.operation("ListObjectsV2").unwrap();
    let mut params = json(r#"{"Bucket": "b"}"#);
    let mut context = CallContext::default();
    s3::before_parameter_build(&op, params.as_map_mut().unwrap(), &mut context).unwrap();
    assert_eq!(
        params.get("EncodingType").and_then(Value::as_str),
        Some("url")
    );
    assert!(context.encoding_type_auto_set);

    let mut parsed = json(
        r#"{"EncodingType": "url", "Prefix": "a%20b", "Contents": [{"Key": "k%2By"}], "CommonPrefixes": [{"Prefix": "p%2F"}]}"#,
    );
    let response = HttpResponse::default();
    s3::after_call(&op, &mut parsed, &response, &context).unwrap();
    assert_eq!(&parsed, golden("list_v2"));

    let mut parsed = json(r#"{"EncodingType": "url", "Contents": [{"Size": 1}]}"#);
    let err = s3::after_call(&op, &mut parsed, &response, &context).unwrap_err();
    assert_eq!(err.to_string(), "'Key'");
}

#[test]
fn json_decode_policies() {
    let model = ServiceModel::load("iam").unwrap();
    for (op, input, key) in [
        (
            "GetRolePolicy",
            r#"{"RoleName": "r", "PolicyName": "p", "PolicyDocument": "%7B%22Version%22%3A%20%222012-10-17%22%2C%20%22N%22%3A%201.50%7D"}"#,
            "iam_get_role_policy",
        ),
        (
            "GetAccountAuthorizationDetails",
            r#"{"RoleDetailList": [{"RoleName": "r", "AssumeRolePolicyDocument": "%5B1%2C%202%5D", "RolePolicyList": [{"PolicyName": "a", "PolicyDocument": "not%20json"}]}]}"#,
            "iam_details",
        ),
    ] {
        let op = model.operation(op).unwrap();
        let mut parsed = json(input);
        decode::json_decode_policies(&mut parsed, op.output());
        assert_eq!(&parsed, golden(key), "{key}");
    }
}

#[test]
fn decode_console_output() {
    let mut parsed = json(r#"{"InstanceId": "i", "Output": "aMOpbGxvCv8="}"#);
    decode::decode_console_output(&mut parsed);
    assert_eq!(&parsed, golden("console_output"));
    let mut parsed = json(r#"{"Output": "aGk"}"#);
    decode::decode_console_output(&mut parsed);
    assert_eq!(&parsed, golden("console_output_bad"));
}

#[test]
fn json_decode_template_body() {
    let mut parsed =
        json(r#"{"TemplateBody": "{\"b\": 1, \"a\": [1.0, \"x\"]}", "StagesAvailable": []}"#);
    decode::json_decode_template_body(&mut parsed);
    assert_eq!(&parsed, golden("template"));
}

#[test]
fn datapipeline_api_to_definition() {
    let mut parsed = json(
        r#"{"pipelineObjects": [{"id": "Default", "name": "Default", "fields": [{"key": "a", "stringValue": "1"}, {"key": "a", "refValue": "x"}, {"key": "a", "stringValue": "2"}, {"key": "b", "refValue": "y"}]}], "parameterObjects": [{"id": "p", "attributes": [{"key": "t", "stringValue": "String"}]}], "parameterValues": [{"id": "v", "stringValue": "1"}, {"id": "v", "stringValue": "2"}], "ResponseMetadata": {}}"#,
    );
    decode::api_to_definition(&mut parsed).unwrap();
    assert_eq!(&parsed, golden("datapipeline"));
}

#[test]
fn s3_error_messages() {
    let model = ServiceModel::load("s3").unwrap();
    let op = model.operation("HeadBucket").unwrap();
    let inputs = [
        r#"{"Error": {"Code": "X", "Message": "Please use AWS4-HMAC-SHA256."}}"#,
        r#"{"Error": {"Code": "PermanentRedirect", "Message": "The bucket must be addressed.", "Endpoint": "b.s3.amazonaws.com"}}"#,
        r#"{"Error": {"Code": "Y", "Message": "AWS KMS managed keys require AWS Signature Version 4"}}"#,
    ];
    let expected = golden("s3errormsg").as_list().unwrap();
    for (input, expected) in inputs.iter().zip(expected) {
        let mut parsed = json(input);
        s3::after_call(
            &op,
            &mut parsed,
            &HttpResponse::default(),
            &CallContext::default(),
        )
        .unwrap();
        assert_eq!(&parsed, expected);
    }
}

#[test]
fn route53_ids() {
    let model = ServiceModel::load("route53").unwrap();
    let op = model.operation("GetHostedZone").unwrap();
    let mut params = json(r#"{"Id": "/hostedzone/Z123"}"#);
    services::fix_route53_ids(&op, params.as_map_mut().unwrap());
    assert_eq!(params, json(r#"{"Id": "Z123"}"#));
    let op = model.operation("GetChange").unwrap();
    let mut params = json(r#"{"Id": "/change/C1"}"#);
    services::fix_route53_ids(&op, params.as_map_mut().unwrap());
    assert_eq!(params, json(r#"{"Id": "C1"}"#));
}

#[test]
fn request_created_handlers() {
    assert_eq!(
        services::switch_host(
            "http://difftest.localhost:1/",
            "https://realtime.example.com"
        ),
        "https://realtime.example.com/"
    );
    assert_eq!(
        services::switch_host_machinelearning("http://a:1/", br#"{"PredictEndpoint": ""}"#),
        None
    );
    assert_eq!(
        services::change_get_to_post(
            "GET",
            "https://h/2013-01-01/search?format=sdk&pretty=true&q=a%20b"
        ),
        Some((
            "https://h/2013-01-01/search".to_string(),
            b"format=sdk&pretty=true&q=a%20b".to_vec()
        ))
    );
}

#[test]
fn glacier_checksums() {
    let model = ServiceModel::load("glacier").unwrap();
    let op = model.operation("UploadArchive").unwrap();
    let mut request = HttpRequest {
        body: test_body(5),
        ..Default::default()
    };
    glacier::before_call(&model, &op, &mut request);
    let tree = s(pairs(golden("tree_hash"))[1].1).to_string();
    assert_eq!(
        request.headers,
        vec![
            ("x-amz-content-sha256".to_string(), tree.clone()),
            ("x-amz-sha256-tree-hash".to_string(), tree),
            (
                "x-amz-glacier-version".to_string(),
                "2012-06-01".to_string()
            ),
        ]
    );
}

fn presign_case(region: &str, signing_name: &str, token: Option<&str>, key: &str, rds: bool) {
    let golden = golden(key).as_list().unwrap();
    let url = format!("https://{signing_name}.{region}.amazonaws.com/");
    let credentials = || {
        Ok(Some(Credentials {
            access_key: "AKID".to_string(),
            secret_key: "SECRET".to_string(),
            token: token.map(str::to_string),
        }))
    };
    let env = PresignEnv {
        url: &url,
        region: Some(region),
        signing_name,
        credentials: &credentials,
        unsigned: false,
        now: chrono::Utc
            .with_ymd_and_hms(2026, 11, 3, 12, 34, 56)
            .unwrap(),
    };
    let mut request = HttpRequest {
        body: s(&golden[0]).as_bytes().to_vec(),
        ..Default::default()
    };
    if rds {
        presign::inject_presigned_url_rds(&mut request, &env).unwrap();
    } else {
        presign::inject_presigned_url_ec2(&mut request, &env).unwrap();
    }
    assert_eq!(String::from_utf8(request.body).unwrap(), s(&golden[1]));
}

#[test]
fn presigned_urls() {
    presign_case("us-east-1", "ec2", None, "presign_ec2", false);
    presign_case("eu-west-1", "rds", Some("TOKEN"), "presign_rds", true);
}

#[test]
fn expires_header() {
    let model = ServiceModel::load("s3").unwrap();
    let op = model.operation("GetObject").unwrap();
    let mut response = HttpResponse {
        status: 200,
        headers: vec![("Expires".to_string(), "not a date".to_string())],
        body: Vec::new(),
    };
    let customized = s3::before_parse(&op, &mut response);
    assert_eq!(
        customized.get("ExpiresString").and_then(Value::as_str),
        Some("not a date")
    );
    assert!(response.headers.is_empty());

    let op = model.operation("ListBuckets").unwrap();
    let mut response = HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: b"<Error><Code>InternalError</Code></Error>".to_vec(),
    };
    s3::before_parse(&op, &mut response);
    assert_eq!(response.status, 500);
}
