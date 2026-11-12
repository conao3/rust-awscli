//! End-to-end tests of `Session::send_with` against a local listener,
//! checked against what the reference CLI sends (captured with a raw
//! socket listener: `Host`, `Accept-Encoding: identity`, the serialized
//! headers, `User-Agent`, the signature headers, `amz-sdk-invocation-id`,
//! `amz-sdk-request`, `Content-Length`).

use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(responses: Vec<&'static [u8]>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        let mut i = 0;
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            loop {
                let mut got = Vec::new();
                let mut buf = vec![0u8; 65536];
                let complete = |got: &[u8]| {
                    let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
                        return false;
                    };
                    let head = String::from_utf8_lossy(&got[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    got.len() >= pos + 4 + len
                };
                while !complete(&got) {
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                if got.is_empty() {
                    break;
                }
                log2.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&got).into_owned());
                let r = responses[i.min(responses.len() - 1)];
                i += 1;
                if s.write_all(r).await.is_err() {
                    break;
                }
            }
        }
    });
    (port, log)
}

fn env(home: &std::path::Path, extra: &[(&str, &str)]) -> HashMap<String, String> {
    let mut e: HashMap<String, String> = [
        ("HOME", home.to_str().unwrap()),
        ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
        ("AWS_SECRET_ACCESS_KEY", "secret"),
        ("AWS_REGION", "us-west-2"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        e.insert(k.to_string(), v.to_string());
    }
    e
}

fn sts_request() -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url_path: "/".into(),
        query: vec![],
        headers: vec![(
            "Content-Type".into(),
            "application/x-www-form-urlencoded; charset=utf-8".into(),
        )],
        body: b"Action=GetCallerIdentity&Version=2011-06-15".to_vec(),
        host_prefix: None,
    }
}

fn header_lines(raw: &str) -> Vec<String> {
    raw.split("\r\n\r\n")
        .next()
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

const ERR500: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nDate: Tue, 03 Nov 2026 00:00:00 GMT\r\n\
Content-Type: text/xml\r\nContent-Length: 97\r\n\r\n<ErrorResponse><Error><Code>InternalFailure</Code><Message>boom</Message></Error></ErrorResponse>";

#[tokio::test]
async fn retries_headers_and_wire_order() {
    let home = tempfile::tempdir().unwrap();
    let (port, log) = serve(vec![ERR500]).await;
    let globals = GlobalArgs {
        endpoint_url: Some(format!("http://127.0.0.1:{port}")),
        ..Default::default()
    };
    let session =
        Session::with_env(&globals, env(home.path(), &[("AWS_MAX_ATTEMPTS", "2")])).unwrap();
    session.set_command("sts", "get-caller-identity");
    let model = ServiceModel::load("sts").unwrap();
    let op = model.operation("GetCallerIdentity").unwrap();
    let endpoint = Endpoint {
        url: format!("http://127.0.0.1:{port}"),
        properties: Default::default(),
        headers: Default::default(),
    };
    let classify = |r: &HttpResponse| (r.status >= 300).then(|| "InternalFailure".to_string());
    let out = session
        .send_with(&model, &op, &endpoint, &sts_request(), &classify)
        .await
        .unwrap();
    assert_eq!(out.response.status, 500);
    assert_eq!(out.attempts, 2);
    assert!(out.max_attempts_reached);
    assert_eq!(
        out.response.header("content-type"),
        Some("text/xml"),
        "response headers keep case"
    );
    assert_eq!(out.response.headers[0].0, "Date");

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    let first = header_lines(&log[0]);
    let names: Vec<&str> = first
        .iter()
        .skip(1)
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(first[0], "POST / HTTP/1.1");
    assert_eq!(
        names,
        [
            "Host",
            "Accept-Encoding",
            "Content-Type",
            "User-Agent",
            "X-Amz-Date",
            "Authorization",
            "amz-sdk-invocation-id",
            "amz-sdk-request",
            "Content-Length"
        ]
    );
    assert_eq!(first[1], format!("Host: 127.0.0.1:{port}"));
    assert!(first.contains(&"amz-sdk-request: attempt=1; max=2".to_string()));
    assert!(first.iter().any(|l| {
        l.starts_with("Authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            && l.contains(
                "/us-west-2/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date, ",
            )
    }));
    assert!(log[0].ends_with("\r\n\r\nAction=GetCallerIdentity&Version=2011-06-15"));
    let second = header_lines(&log[1]);
    let req_hdr = second
        .iter()
        .find(|l| l.starts_with("amz-sdk-request: "))
        .unwrap();
    assert!(req_hdr.starts_with("amz-sdk-request: ttl=20261103T0001"));
    assert!(req_hdr.ends_with("; attempt=2; max=2"));
    let id = |lines: &[String]| {
        lines
            .iter()
            .find(|l| l.starts_with("amz-sdk-invocation-id: "))
            .cloned()
    };
    assert_eq!(id(&first), id(&second));
    let ua = first
        .iter()
        .find(|l| l.starts_with("User-Agent: "))
        .unwrap();
    assert!(ua.contains(" md/command#sts.get-caller-identity"));
}

#[tokio::test]
async fn single_attempt_reaches_max_and_connection_errors() {
    let home = tempfile::tempdir().unwrap();
    let globals = GlobalArgs::default();
    let session =
        Session::with_env(&globals, env(home.path(), &[("AWS_MAX_ATTEMPTS", "1")])).unwrap();
    let model = ServiceModel::load("sts").unwrap();
    let op = model.operation("GetCallerIdentity").unwrap();
    let endpoint = Endpoint {
        url: "http://127.0.0.1:9".into(),
        properties: Default::default(),
        headers: Default::default(),
    };
    let err = session
        .send_with(&model, &op, &endpoint, &sts_request(), &|_| None)
        .await
        .err()
        .unwrap();
    let err = err.downcast::<SessionError>().unwrap();
    assert_eq!(err.kind, ErrorKind::Connection);
    assert_eq!(
        err.to_cli_error().message,
        "\nawsr: [ERROR]: Could not connect to the endpoint URL: \"http://127.0.0.1:9/\"\n"
    );
    assert_eq!(err.exit_code() as i32, 255);
}

#[tokio::test]
async fn configuration_errors() {
    let home = tempfile::tempdir().unwrap();
    let model = ServiceModel::load("sts").unwrap();
    let op = model.operation("GetCallerIdentity").unwrap();
    let endpoint = Endpoint {
        url: "http://127.0.0.1:9".into(),
        properties: Default::default(),
        headers: Default::default(),
    };
    let cases: &[(&[(&str, &str)], &str)] = &[
        (
            &[("AWS_RETRY_MODE", "legacy")],
            "Invalid value provided to \"mode\": \"legacy\" must be one of: \"standard\" or \"adaptive\"",
        ),
        (
            &[("AWS_MAX_ATTEMPTS", "0")],
            "Value provided to \"max_attempts\": 0 must be an integer greater than or equal to one.",
        ),
        (
            &[("AWS_MAX_ATTEMPTS", "x")],
            "invalid literal for int() with base 10: 'x'",
        ),
        (
            &[("AWS_REGION", "us_west")],
            "Provided region_name 'us_west' doesn't match a supported format.",
        ),
    ];
    for (extra, message) in cases {
        let session = Session::with_env(&GlobalArgs::default(), env(home.path(), extra)).unwrap();
        let err = session
            .send_with(&model, &op, &endpoint, &sts_request(), &|_| None)
            .await
            .err()
            .unwrap()
            .downcast::<SessionError>()
            .unwrap();
        assert_eq!(err.message, *message);
        assert_eq!(err.exit_code() as i32, 255);
    }

    // No credentials anywhere: exit 253.
    let mut e = env(home.path(), &[]);
    e.remove("AWS_ACCESS_KEY_ID");
    e.remove("AWS_SECRET_ACCESS_KEY");
    let session = Session::with_env(&GlobalArgs::default(), e).unwrap();
    let err = session
        .send_with(&model, &op, &endpoint, &sts_request(), &|_| None)
        .await
        .err()
        .unwrap()
        .downcast::<SessionError>()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::NoCredentials);
    assert_eq!(err.exit_code() as i32, 253);

    // `--profile` naming a missing profile.
    let session = Session::with_env(
        &GlobalArgs {
            profile: Some("nope".into()),
            ..Default::default()
        },
        env(home.path(), &[]),
    )
    .unwrap();
    let err = session
        .send_with(&model, &op, &endpoint, &sts_request(), &|_| None)
        .await
        .err()
        .unwrap()
        .downcast::<SessionError>()
        .unwrap();
    assert_eq!(
        err.to_cli_error().message,
        "\nawsr: [ERROR]: The config profile (nope) could not be found\n"
    );

    // A broken config file fails when the session is created.
    let aws = home.path().join(".aws");
    std::fs::create_dir_all(&aws).unwrap();
    std::fs::write(aws.join("config"), "region=us-east-1\n").unwrap();
    let err = Session::with_env(&GlobalArgs::default(), env(home.path(), &[]))
        .err()
        .unwrap();
    assert_eq!(
        err.message,
        format!(
            "Unable to parse config file: {}",
            aws.join("config").display()
        )
    );
}

#[tokio::test]
async fn unsigned_request_has_no_auth_headers() {
    let home = tempfile::tempdir().unwrap();
    let ok: &'static [u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    let (port, log) = serve(vec![ok]).await;
    let globals = GlobalArgs {
        no_sign_request: true,
        ..Default::default()
    };
    let session = Session::with_env(&globals, env(home.path(), &[])).unwrap();
    let model = ServiceModel::load("sts").unwrap();
    let op = model.operation("GetCallerIdentity").unwrap();
    let endpoint = Endpoint {
        url: format!("http://127.0.0.1:{port}/base/"),
        properties: Default::default(),
        headers: [(
            "x-extra".to_string(),
            vec!["1".to_string(), "2".to_string()],
        )]
        .into_iter()
        .collect(),
    };
    let out = session
        .send_with(&model, &op, &endpoint, &sts_request(), &|_| None)
        .await
        .unwrap();
    assert_eq!(out.response.status, 200);
    assert_eq!(out.attempts, 1);
    assert!(!out.max_attempts_reached);
    let log = log.lock().unwrap();
    let lines = header_lines(&log[0]);
    assert_eq!(lines[0], "POST /base/ HTTP/1.1");
    assert!(lines.contains(&"x-extra: 1".to_string()));
    assert!(!lines.iter().any(|l| l.starts_with("Authorization")));
    assert!(!lines.iter().any(|l| l.starts_with("X-Amz-Date")));
}
