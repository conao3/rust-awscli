use std::path::PathBuf;

use serde_json::json;

use super::*;

fn params_from_json(v: &Json) -> IndexMap<String, ParamValue> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    let pv = match v {
                        Json::String(s) => ParamValue::String(s.clone()),
                        Json::Bool(b) => ParamValue::Bool(*b),
                        Json::Array(a) => ParamValue::StringArray(
                            a.iter()
                                .map(|x| x.as_str().unwrap_or_default().to_string())
                                .collect(),
                        ),
                        other => panic!("unsupported test param {other}"),
                    };
                    (k.clone(), pv)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn headers_from_json(v: Option<&Json>) -> IndexMap<String, Vec<String>> {
    v.and_then(Json::as_object)
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    let vals = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_str().unwrap().to_string())
                        .collect();
                    (k.clone(), vals)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn suite_dirs() -> Option<Vec<PathBuf>> {
    let root = std::env::var("AWSR_REFERENCE_SRC").ok()?;
    let dir = PathBuf::from(root).join("tests/functional/botocore/endpoint-rules");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("endpoint-tests-1.json").is_file())
        .collect();
    dirs.sort();
    Some(dirs)
}

/// `tests/functional/botocore/test_endpoint_rulesets.py`: the provider
/// test cases of every `endpoint-tests-1.json` (endpoints and errors) and
/// the end-to-end cases that yield endpoints.
#[test]
fn reference_endpoint_suites() {
    let Some(dirs) = suite_dirs() else {
        eprintln!("AWSR_REFERENCE_SRC not set; skipping endpoint suites");
        return;
    };
    let mut failures = Vec::new();
    let (mut cases, mut e2e_cases) = (0usize, 0usize);
    for dir in dirs {
        let service = dir.file_name().unwrap().to_str().unwrap().to_string();
        if !crate::model::has_service(&service) {
            continue;
        }
        let model = ServiceModel::load(&service).unwrap();
        let Some(ruleset) = model.ruleset() else {
            continue;
        };
        let text = std::fs::read_to_string(dir.join("endpoint-tests-1.json")).unwrap();
        let data: Json = serde_json::from_str(&text).unwrap();
        for case in data["testCases"].as_array().unwrap() {
            cases += 1;
            let doc = case["documentation"].as_str().unwrap_or_default();
            let params = params_from_json(case.get("params").unwrap_or(&json!({})));
            let expect = &case["expect"];
            let result = resolve_endpoint(ruleset, &params);
            if let Some(ep) = expect.get("endpoint") {
                match &result {
                    Ok(got) => {
                        let want_props = ep
                            .get("properties")
                            .and_then(Json::as_object)
                            .cloned()
                            .unwrap_or_default();
                        if got.url != ep["url"].as_str().unwrap()
                            || got.properties != want_props
                            || got.headers != headers_from_json(ep.get("headers"))
                        {
                            failures.push(format!("{service}: {doc}: got {got:?}, want {ep}"));
                        }
                    }
                    Err(e) => failures.push(format!("{service}: {doc}: unexpected error {e}")),
                }
            } else if let Some(err) = expect.get("error") {
                match &result {
                    Err(EndpointError::Resolution(msg)) if msg == err.as_str().unwrap() => {}
                    other => failures.push(format!("{service}: {doc}: got {other:?}, want {err}")),
                }
            }
            if expect.get("endpoint").is_none() {
                continue;
            }
            for inputs in case
                .get("operationInputs")
                .and_then(Json::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let op_params =
                    Value::from_json(inputs.get("operationParams").unwrap_or(&json!({})));
                if matches!(
                    op_params.get("Bucket").and_then(Value::as_str),
                    Some("bucket name" | "example.com#")
                ) {
                    continue;
                }
                e2e_cases += 1;
                let op_name = inputs["operationName"].as_str().unwrap();
                let op = model.operation(op_name).unwrap();
                let mut builtins = Builtins::default();
                if let Some(b) = inputs.get("builtInParams").and_then(Json::as_object) {
                    for (k, v) in b {
                        builtins
                            .set(k, ParamValue::from_node(Node::leak(v)))
                            .unwrap();
                    }
                }
                let params = provider_params(&model, &op, &op_params, &builtins);
                let want = expect["endpoint"]["url"].as_str().unwrap();
                match resolve_endpoint(ruleset, &params) {
                    Ok(got) if want.starts_with(&got.url) || got.url.starts_with(want) => {}
                    other => failures.push(format!(
                        "{service}: e2e {op_name} {doc}: got {other:?}, want {want} (params {params:?})"
                    )),
                }
            }
        }
    }
    eprintln!("endpoint suites: {cases} cases, {e2e_cases} e2e cases");
    assert!(cases > 10_000, "only {cases} cases");
    assert!(e2e_cases > 300, "only {e2e_cases} e2e cases");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn ruleset(rules: Json, parameters: Json) -> &'static Node {
    Node::leak(&json!({"version": "1.0", "parameters": parameters, "rules": rules}))
}

#[test]
fn error_messages_match_python() {
    let rs = ruleset(
        json!([{"conditions": [{"fn": "isSet", "argv": [{"ref": "Region"}]}],
               "endpoint": {"url": "https://x"}, "type": "endpoint"}]),
        json!({"Region": {"type": "String"}, "Flag": {"type": "Boolean"},
               "Arr": {"type": "stringArray"}}),
    );
    let mut p = IndexMap::new();
    p.insert("Flag".into(), ParamValue::Bool(true));
    p.insert("Arr".into(), ParamValue::StringArray(vec!["a".into()]));
    let err = resolve_endpoint(rs, &p).unwrap_err();
    assert_eq!(
        err.to_string(),
        "No endpoint found for parameters:\nFlag: True\nArr: ('a',)"
    );
    let mut p = IndexMap::new();
    p.insert("Region".into(), ParamValue::Bool(true));
    assert_eq!(
        resolve_endpoint(rs, &p).unwrap_err().to_string(),
        "Value (Region) is the wrong type. Must be <class 'str'>."
    );
}

#[test]
fn string_equals_type_error() {
    let rs = ruleset(
        json!([{"conditions": [{"fn": "stringEquals", "argv": [{"ref": "Region"}, "x"]}],
               "endpoint": {"url": "https://x"}, "type": "endpoint"}]),
        json!({"Region": {"type": "String"}}),
    );
    assert_eq!(
        resolve_endpoint(rs, &IndexMap::new())
            .unwrap_err()
            .to_string(),
        "Both values must be strings, not <class 'NoneType'> and <class 'str'>."
    );
}

#[test]
fn build_params_for_operations() {
    let s3 = ServiceModel::load("s3").unwrap();
    let op = s3.operation("GetBucketLocation").unwrap();
    let builtins = Builtins::compute(&BuiltinInputs {
        region_name: Some("us-west-2".into()),
        endpoint_prefix: "s3".into(),
        ..Default::default()
    });
    let call = Value::parse_json(r#"{"Bucket": "my-bucket"}"#).unwrap();
    let params = build_params(&s3, &op, &call, &builtins);
    assert_eq!(
        params.get("Bucket"),
        Some(&ParamValue::String("my-bucket".into()))
    );
    assert_eq!(params.get("ForcePathStyle"), Some(&ParamValue::Bool(true)));
    assert_eq!(params.get("UseArnRegion"), Some(&ParamValue::Bool(true)));
    let ep = construct_endpoint(&s3, &op, &call, &builtins, true).unwrap();
    assert_eq!(ep.url, "https://s3.us-west-2.amazonaws.com/my-bucket");
    let ctx = signing_context(&ep).unwrap().unwrap();
    assert_eq!(ctx.scheme.as_deref(), Some("v4"));
    assert_eq!(ctx.signing_name.as_deref(), Some("s3"));
    assert_eq!(ctx.signing_region.as_deref(), Some("us-west-2"));
    assert_eq!(ctx.disable_double_encoding, Some(true));

    let ddb = ServiceModel::load("dynamodb").unwrap();
    let op = ddb.operation("BatchGetItem").unwrap();
    let call = Value::parse_json(r#"{"RequestItems": {"t1": {}, "t2": {}}}"#).unwrap();
    let params = build_params(&ddb, &op, &call, &builtins);
    assert_eq!(
        params.get("ResourceArnList"),
        Some(&ParamValue::StringArray(vec!["t1".into(), "t2".into()]))
    );
}

#[test]
fn ruleset_errors_are_translated() {
    let mut p = IndexMap::new();
    p.insert("Bucket".into(), ParamValue::String("arn:x".into()));
    let e = ruleset_error_to_botocore_exception(
        EndpointError::Resolution("Invalid ARN: bad".into()),
        "s3control",
        &p,
    );
    assert_eq!(
        e.to_string(),
        "S3 ARN \"arn:x\" provided is invalid for this operation. Invalid ARN: bad"
    );
    let e = ruleset_error_to_botocore_exception(
        EndpointError::Resolution("Invalid region in ARN: `us-we$t-2` (invalid DNS name)".into()),
        "s3",
        &p,
    );
    assert_eq!(
        e.to_string(),
        "Invalid host label to be bound to the hostname of the endpoint: \"us-we$t-2\"."
    );
    let e = ruleset_error_to_botocore_exception(
        EndpointError::Resolution("Invalid ARN: x".into()),
        "s3",
        &p,
    );
    assert_eq!(e.exit_code(), 252);
}

#[test]
fn signing_context_selection() {
    let ep = Endpoint {
        url: "https://x".into(),
        properties: json!({"authSchemes": [
            {"name": "sigv4a", "signingName": "s3", "signingRegionSet": ["*"]},
            {"name": "sigv4", "signingName": "s3", "signingRegion": "us-east-1"}
        ]})
        .as_object()
        .unwrap()
        .clone(),
        headers: IndexMap::new(),
    };
    let ctx = signing_context(&ep).unwrap().unwrap();
    assert_eq!(ctx.scheme.as_deref(), Some("v4a"));
    assert_eq!(ctx.signing_region.as_deref(), Some("*"));
    let ctx = signing_context_with(
        &ep,
        &AuthOptions {
            signature_version: Some("s3v4".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(ctx.scheme.as_deref(), Some("s3v4"));
    assert_eq!(ctx.signing_region.as_deref(), Some("us-east-1"));
    let ctx = signing_context_with(
        &ep,
        &AuthOptions {
            auth_scheme_preference: Some("sigv4".into()),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(ctx.scheme.as_deref(), Some("v4"));
    let ctx = signing_context_with(
        &ep,
        &AuthOptions {
            unsigned: true,
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(ctx.scheme.as_deref(), Some("none"));
}

#[test]
fn xform_names() {
    assert_eq!(xform_name("ForcePathStyle", "_"), "force_path_style");
    assert_eq!(
        xform_name("DisableS3ExpressSessionAuth", "_"),
        "disable_s3_express_session_auth"
    );
    assert_eq!(xform_name("ListWebACLs", "_"), "list_web_acls");
    assert_eq!(
        xform_name("aws.isVirtualHostableS3Bucket", "_"),
        "aws.is_virtual_hostable_s3_bucket"
    );
}
