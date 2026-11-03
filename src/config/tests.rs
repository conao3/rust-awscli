//! Configuration goldens from the reference CLI's session
//! (`testdata/config_golden.py`).

use super::*;

fn expected_value(v: &serde_json::Value) -> Result<Option<String>, String> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(b) => Ok(Some(b.to_string())),
        serde_json::Value::Number(n) => Ok(Some(n.to_string())),
        serde_json::Value::String(s) => Ok(Some(s.clone())),
        serde_json::Value::Object(o) if o.contains_key("error") => {
            Err(o["error"].as_str().unwrap().to_string())
        }
        other => panic!("unexpected golden value {other}"),
    }
}

#[test]
fn matches_reference_session() {
    for line in include_str!("testdata/config.jsonl").lines() {
        let case: serde_json::Value = serde_json::from_str(line).unwrap();
        let name = case["name"].as_str().unwrap();
        let home = tempfile::tempdir().unwrap();
        let aws = home.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        if let Some(text) = case["config"].as_str() {
            std::fs::write(aws.join("config"), text).unwrap();
        }
        if let Some(text) = case["credentials"].as_str() {
            std::fs::write(aws.join("credentials"), text).unwrap();
        }
        if let Some(files) = case["files"].as_object() {
            for (rel, text) in files {
                let p = home.path().join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text.as_str().unwrap()).unwrap();
            }
        }
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("HOME".into(), home.path().to_string_lossy().into_owned());
        env.insert("AWS_EC2_METADATA_DISABLED".into(), "true".into());
        for (k, v) in case["env"].as_object().unwrap() {
            env.insert(k.clone(), v.as_str().unwrap().to_string());
        }
        let globals = GlobalArgs {
            profile: case["profile"].as_str().map(str::to_string),
            region: case["region"].as_str().map(str::to_string),
            ..Default::default()
        };
        let config = Config::with_env(&globals, env).unwrap();
        for (var, expected) in case["expected"].as_object().unwrap() {
            if var == "s3" {
                match expected {
                    serde_json::Value::Object(o) if !o.contains_key("error") => {
                        for (k, v) in o {
                            let got = config.get(&format!("s3.{k}")).map_err(|e| e.message);
                            assert_eq!(got, expected_value(v), "{name}: s3.{k}");
                        }
                    }
                    serde_json::Value::Null => {
                        assert_eq!(config.get("s3.addressing_style").unwrap(), None, "{name}");
                    }
                    _ => {
                        assert!(config.get("s3.addressing_style").is_err(), "{name}");
                    }
                }
                continue;
            }
            let got = config.get(var).map_err(|e| e.message);
            assert_eq!(got, expected_value(expected), "{name}: {var}");
        }
    }
}

#[test]
fn lookup_falls_back_on_missing_profile() {
    let home = tempfile::tempdir().unwrap();
    let env: HashMap<String, String> = [(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]
    .into();
    let globals = GlobalArgs {
        profile: Some("nope".into()),
        cli_error_format: Some("JSON".into()),
        ..Default::default()
    };
    let c = Config::with_env(&globals, env).unwrap();
    assert_eq!(
        c.config_variable("cli_binary_format").as_deref(),
        Some("base64")
    );
    assert_eq!(
        c.config_variable("cli_timestamp_format").as_deref(),
        Some("iso8601")
    );
    assert_eq!(
        c.config_variable("cli_error_format").as_deref(),
        Some("json")
    );
}

#[test]
fn configured_endpoint_urls() {
    let home = tempfile::tempdir().unwrap();
    let aws = home.path().join(".aws");
    std::fs::create_dir_all(&aws).unwrap();
    std::fs::write(
        aws.join("config"),
        "[default]\nservices = dev\nendpoint_url = http://global\n\
         [services dev]\ndynamodb =\n  endpoint_url = http://ddb\n",
    )
    .unwrap();
    let mut env: HashMap<String, String> = [(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]
    .into();
    let c = Config::with_env(&GlobalArgs::default(), env.clone()).unwrap();
    assert_eq!(
        c.configured_endpoint_url("dynamodb").unwrap().as_deref(),
        Some("http://ddb")
    );
    assert_eq!(
        c.configured_endpoint_url("sts").unwrap().as_deref(),
        Some("http://global")
    );
    env.insert(
        "AWS_ENDPOINT_URL_SECRETS_MANAGER".into(),
        "http://sm".into(),
    );
    let c = Config::with_env(&GlobalArgs::default(), env).unwrap();
    assert_eq!(
        c.configured_endpoint_url("secretsmanager")
            .unwrap()
            .as_deref(),
        Some("http://sm")
    );
}
