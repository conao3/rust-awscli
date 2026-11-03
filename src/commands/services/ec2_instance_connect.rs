//! `ec2-instance-connect ssh` and `open-tunnel`
//! (`awscli/customizations/ec2instanceconnect/ssh.py`, `opentunnel.py`,
//! `eicefetcher.py`, `eicesigner.py`, `websocket.py`).

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::sync::Arc;

use base64::Engine as _;
use chrono::Utc;

use super::basic::{self, ArgDef, Args};
use super::exec::Client;
use super::presign::{self, PresignRequest, presign_url};
use crate::cli::argparse::NsVal;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "ssh" => ssh(ctx, args),
        "open-tunnel" => open_tunnel(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn invalid(ctx: &Context, message: &str) -> CliError {
    basic::param_validation(ctx, message)
}

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

fn eice_option<'a>(args: &'a Args, key: &str) -> Option<&'a Value> {
    args.get("eice-options")
        .filter(|v| v.is_truthy())
        .and_then(|o| o.get(key))
        .filter(|v| !matches!(v, Value::Null))
}

/// `describe_instances(InstanceIds=[id])['Reservations'][0]['Instances'][0]`.
fn instance_metadata(ctx: &Context, ec2: &Client<'_>, id: &str) -> Result<Value, CliError> {
    let mut params = Map::new();
    params.insert("InstanceIds".into(), Value::List(vec![Value::str(id)]));
    let r = ec2.call("DescribeInstances", &Value::Map(params))?;
    r.get("Reservations")
        .and_then(Value::as_list)
        .and_then(|l| l.first())
        .and_then(|r| r.get("Instances"))
        .and_then(Value::as_list)
        .and_then(|l| l.first())
        .cloned()
        .ok_or_else(|| general(ctx, "list index out of range"))
}

fn state_filter() -> Value {
    let mut f = Map::new();
    f.insert("Name".into(), Value::str("state"));
    f.insert(
        "Values".into(),
        Value::List(
            [
                "create-complete",
                "update-in-progress",
                "update-failed",
                "update-complete",
            ]
            .iter()
            .map(|s| Value::str(*s))
            .collect(),
        ),
    );
    Value::Map(f)
}

/// `InstanceConnectEndpointRequestFetcher.get_available_instance_connect_endpoint`.
fn find_endpoint(
    ctx: &Context,
    ec2: &Client<'_>,
    vpc_id: Option<&Value>,
    subnet_id: Option<&Value>,
    endpoint_id: Option<&str>,
) -> Result<Value, CliError> {
    if let Some(id) = endpoint_id {
        let mut params = Map::new();
        params.insert("Filters".into(), Value::List(vec![state_filter()]));
        params.insert(
            "InstanceConnectEndpointIds".into(),
            Value::List(vec![Value::str(id)]),
        );
        let r = ec2.call("DescribeInstanceConnectEndpoints", &Value::Map(params))?;
        return r
            .get("InstanceConnectEndpoints")
            .and_then(Value::as_list)
            .and_then(|l| l.first())
            .cloned()
            .ok_or_else(|| {
                invalid(
                    ctx,
                    &format!("There are no available instance connect endpoints with {id}"),
                )
            });
    }
    let mut vpc = Map::new();
    vpc.insert("Name".into(), Value::str("vpc-id"));
    vpc.insert(
        "Values".into(),
        Value::List(vec![vpc_id.cloned().unwrap_or(Value::Null)]),
    );
    let mut params = Map::new();
    params.insert(
        "Filters".into(),
        Value::List(vec![state_filter(), Value::Map(vpc)]),
    );
    let mut endpoints = Vec::new();
    for page in ec2.paginate("DescribeInstanceConnectEndpoints", &Value::Map(params))? {
        if let Some(Value::List(l)) = page.get("InstanceConnectEndpoints") {
            endpoints.extend(l.iter().cloned());
        }
    }
    if !endpoints.is_empty() {
        if let Some(e) = endpoints
            .iter()
            .find(|e| e.get("SubnetId") == subnet_id && subnet_id.is_some())
        {
            return Ok(e.clone());
        }
        return Ok(endpoints[0].clone());
    }
    Err(invalid(
        ctx,
        "There are no available instance connect endpoints.",
    ))
}

/// `InstanceConnectEndpointRequestFetcher.get_eice_dns_name`.
fn eice_dns_name(ctx: &Context, eice: &Value) -> Result<String, CliError> {
    let fips = ctx
        .session
        .config()
        .get_bool("use_fips_endpoint")
        .ok()
        .flatten()
        .unwrap_or(false);
    if !fips {
        return eice
            .get("DnsName")
            .map(crate::pyfmt::py_str)
            .ok_or_else(|| general(ctx, "'DnsName'"));
    }
    match eice.get("FipsDnsName").filter(|v| v.is_truthy()) {
        Some(v) => Ok(crate::pyfmt::py_str(v)),
        None => Err(general(ctx, "Unable to find FIPS Endpoint")),
    }
}

fn ssh_defs() -> Vec<ArgDef> {
    vec![
        ArgDef::new("instance-id").required(),
        ArgDef::new("instance-ip"),
        ArgDef::new("private-key-file"),
        ArgDef::new("os-user").default_str("ec2-user"),
        ArgDef::new("ssh-port").int().default_str("22"),
        ArgDef::new("local-forwarding"),
        ArgDef::new("connection-type")
            .choices(&["auto", "direct", "eice"])
            .default_str("auto"),
        ArgDef::new("eice-options").schema(serde_json::json!({
            "type": "object",
            "properties": {
                "maxTunnelDuration": {"type": "integer", "required": false},
                "endpointId": {"type": "string", "required": false},
                "dnsName": {"type": "string", "required": false}
            }
        })),
    ]
}

/// `SshCommand._validate_parsed_args`.
fn validate_ssh(ctx: &Context, a: &Args) -> Result<(), CliError> {
    let id = a.str("instance-id").unwrap_or_default();
    if !id.is_empty()
        && !regex::Regex::new("^i-[0-9a-zA-Z]+$")
            .expect("re")
            .is_match(&id)
    {
        return Err(invalid(
            ctx,
            "The specified instance ID is invalid. Provide the full instance ID in the form i-xxxxxxxxxxxxxxxxx.",
        ));
    }
    let options = a.get("eice-options").filter(|v| v.is_truthy());
    let connection = a.str("connection-type").unwrap_or_default();
    if connection == "direct" && options.is_some() {
        return Err(invalid(
            ctx,
            "eice-options can't be specified when connection type is direct.",
        ));
    }
    let truthy = |k: &str| eice_option(a, k).is_some_and(Value::is_truthy);
    if truthy("dnsName") && !truthy("endpointId") {
        return Err(invalid(
            ctx,
            "When specifying dnsName, you must specify endpointId.",
        ));
    }
    if let Some(d) = eice_option(a, "maxTunnelDuration").and_then(Value::as_i64)
        && !(1..=3600).contains(&d)
    {
        return Err(invalid(
            ctx,
            "Invalid value specified for maxTunnelDuration. Value must be greater than 1 and less than 3600.",
        ));
    }
    if truthy("endpointId") {
        let e = eice_option(a, "endpointId")
            .map(crate::pyfmt::py_str)
            .unwrap_or_default();
        if !regex::Regex::new("^eice-[0-9a-zA-Z_]+$")
            .expect("re")
            .is_match(&e)
        {
            return Err(invalid(
                ctx,
                "The specified endpointId is invalid. Provide the full EC2 Instance Connect Endpoint ID in the form eice-xxxxxxxxxxxxxxxxx.",
            ));
        }
    }
    if truthy("dnsName") {
        let d = eice_option(a, "dnsName")
            .map(crate::pyfmt::py_str)
            .unwrap_or_default();
        if !regex::Regex::new("^[0-9a-zA-Z.-]+$")
            .expect("re")
            .is_match(&d)
        {
            return Err(invalid(ctx, "The specified dnsName is invalid."));
        }
    }
    if a.get("instance-ip").is_some_and(Value::is_truthy) && connection == "auto" {
        return Err(invalid(
            ctx,
            "When specifying instance-ip, you must specify connection-type.",
        ));
    }
    Ok(())
}

/// `shlex.quote`.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `shutil.which`.
fn which(name: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    use std::os::unix::fs::PermissionsExt as _;
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(name);
        if let Ok(m) = std::fs::metadata(&p)
            && m.is_file()
            && m.permissions().mode() & 0o111 != 0
        {
            return Some(p.display().to_string());
        }
    }
    None
}

/// `SshCommand._run_main`.
fn ssh(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = ssh_defs();
    let Some(a) = basic::parse(ctx, &["ec2-instance-connect", "ssh"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    validate_ssh(ctx, &a)?;
    let ec2 = Client::new(ctx, "ec2")?;
    let instance_id = a.str("instance-id").unwrap_or_default();
    let meta = instance_metadata(ctx, &ec2, &instance_id)?;
    let field = |k: &str| {
        meta.get(k)
            .filter(|v| v.is_truthy())
            .map(crate::pyfmt::py_str)
    };
    let (private_ip, public_ip, ipv6) = (
        field("PrivateIpAddress"),
        field("PublicIpAddress"),
        field("Ipv6Address"),
    );
    let connection = a.str("connection-type").unwrap_or_default();
    let options = a.get("eice-options").filter(|v| v.is_truthy());
    let (use_tunnel, ip) = if let Some(ip) = a.str("instance-ip").filter(|s| !s.is_empty()) {
        (connection == "eice", Some(ip))
    } else if connection == "eice" || options.is_some() {
        (true, private_ip.clone().or(ipv6.clone()))
    } else if connection == "direct" {
        (
            false,
            public_ip.clone().or(ipv6.clone()).or(private_ip.clone()),
        )
    } else if public_ip.is_some() {
        (false, public_ip.clone())
    } else if private_ip.is_some() {
        (true, private_ip.clone())
    } else {
        (false, ipv6.clone())
    };
    let Some(ip) = ip else {
        return Err(invalid(
            ctx,
            "Unable to find any IP address on the instance to connect to.",
        ));
    };
    let mut eice_id = eice_option(&a, "endpointId").map(crate::pyfmt::py_str);
    let mut eice_dns = eice_option(&a, "dnsName")
        .filter(|v| v.is_truthy())
        .map(crate::pyfmt::py_str);
    if use_tunnel && eice_dns.is_none() {
        let eice = find_endpoint(
            ctx,
            &ec2,
            meta.get("VpcId"),
            meta.get("SubnetId"),
            eice_id.as_deref().filter(|s| !s.is_empty()),
        )?;
        eice_id = eice
            .get("InstanceConnectEndpointId")
            .map(crate::pyfmt::py_str);
        eice_dns = Some(eice_dns_name(ctx, &eice)?);
    }
    let os_user = a.str("os-user").unwrap_or_default();
    let ssh_port = a.str("ssh-port").unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("tmp{:016x}", rand::random::<u64>()));
    let key_file = match a.str("private-key-file").filter(|s| !s.is_empty()) {
        Some(k) => k,
        None => {
            let (private_pem, public_key) = generate_key();
            let eic = Client::new(ctx, "ec2-instance-connect")?;
            let mut params = Map::new();
            params.insert("InstanceId".into(), Value::str(&instance_id));
            params.insert("InstanceOSUser".into(), Value::str(&os_user));
            params.insert("SSHPublicKey".into(), Value::str(public_key));
            eic.call("SendSSHPublicKey", &Value::Map(params))?;
            use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&tmp)
                .map_err(|e| general(ctx, &e.to_string()))?;
            let path = tmp.join("private-key");
            std::fs::write(&path, private_pem).map_err(|e| general(ctx, &e.to_string()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
                .map_err(|e| general(ctx, &e.to_string()))?;
            path.display().to_string()
        }
    };
    let result = run_ssh(
        ctx,
        &a,
        use_tunnel,
        &instance_id,
        &ssh_port,
        &os_user,
        &key_file,
        &ip,
        eice_id.as_deref(),
        eice_dns.as_deref(),
    );
    let _ = std::fs::remove_dir_all(&tmp);
    match result? {
        Ok(code) => Ok(code),
        Err(code) => std::process::exit(code),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ssh(
    ctx: &Context,
    a: &Args,
    use_tunnel: bool,
    instance_id: &str,
    ssh_port: &str,
    os_user: &str,
    key_file: &str,
    ip: &str,
    eice_id: Option<&str>,
    eice_dns: Option<&str>,
) -> Result<Result<ExitCode, i32>, CliError> {
    let aws = std::env::args().next().unwrap_or_else(|| "aws".to_string());
    let mut proxy = vec![
        aws,
        "ec2-instance-connect".to_string(),
        "open-tunnel".to_string(),
        "--instance-id".to_string(),
        instance_id.to_string(),
        "--private-ip-address".to_string(),
        ip.to_string(),
        "--remote-port".to_string(),
        ssh_port.to_string(),
    ];
    if let Some(r) = ctx.globals.region.as_deref().filter(|s| !s.is_empty()) {
        proxy.extend(["--region".to_string(), r.to_string()]);
    }
    if let Some(p) = ctx.globals.profile.as_deref().filter(|s| !s.is_empty()) {
        proxy.extend(["--profile".to_string(), p.to_string()]);
    }
    if let Some(e) = eice_id.filter(|s| !s.is_empty()) {
        proxy.extend(["--instance-connect-endpoint-id".to_string(), e.to_string()]);
    }
    if let Some(d) = eice_dns.filter(|s| !s.is_empty()) {
        proxy.extend([
            "--instance-connect-endpoint-dns-name".to_string(),
            d.to_string(),
        ]);
    }
    if let Some(m) = eice_option(a, "maxTunnelDuration").filter(|v| v.is_truthy()) {
        proxy.extend(["--max-tunnel-duration".to_string(), crate::pyfmt::py_str(m)]);
    }
    let Some(ssh_path) = which("ssh") else {
        return Err(ctx.structured_error(
            ExitCode::Config,
            "Configuration",
            "SSH not available. Please refer to the documentation at https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/Connect-using-EC2-Instance-Connect-Endpoint.html.",
        ));
    };
    let mut command = vec![
        ssh_path,
        "-o".to_string(),
        "ServerAliveInterval=5".to_string(),
        "-p".to_string(),
        ssh_port.to_string(),
        "-i".to_string(),
        key_file.to_string(),
    ];
    if let Some(l) = a.str("local-forwarding").filter(|s| !s.is_empty()) {
        command.extend(["-L".to_string(), l]);
    }
    if ctx.globals.debug {
        command.push("-v".to_string());
    }
    if use_tunnel {
        command.push("-o".to_string());
        command.push(format!(
            "ProxyCommand={}",
            proxy
                .iter()
                .map(|p| shell_quote(p))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    command.push(format!("{os_user}@{ip}"));
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .map_err(|e| general(ctx, &e.to_string()))?;
    // `return rc`: the CLI exits with ssh's exit status.
    Ok(match status.code() {
        Some(0) => Ok(ExitCode::Success),
        Some(code) => Err(code),
        None => Err(255),
    })
}

fn ssh_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// `KeyManager`: a new Ed25519 key as (OpenSSH private key PEM, public key
/// line).
fn generate_key() -> (String, String) {
    use ring::signature::KeyPair as _;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("ed25519 key");
    let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("ed25519 key");
    let public = pair.public_key().as_ref().to_vec();
    let seed = pkcs8.as_ref()[16..48].to_vec();
    let mut pub_blob = Vec::new();
    ssh_string(&mut pub_blob, b"ssh-ed25519");
    ssh_string(&mut pub_blob, &public);
    let check = rand::random::<u32>().to_be_bytes();
    let mut private = Vec::new();
    private.extend_from_slice(&check);
    private.extend_from_slice(&check);
    ssh_string(&mut private, b"ssh-ed25519");
    ssh_string(&mut private, &public);
    let mut secret = seed;
    secret.extend_from_slice(&public);
    ssh_string(&mut private, &secret);
    ssh_string(&mut private, b"");
    let mut pad = 1u8;
    while private.len() % 8 != 0 {
        private.push(pad);
        pad += 1;
    }
    let mut blob = b"openssh-key-v1\0".to_vec();
    ssh_string(&mut blob, b"none");
    ssh_string(&mut blob, b"none");
    ssh_string(&mut blob, b"");
    blob.extend_from_slice(&1u32.to_be_bytes());
    ssh_string(&mut blob, &pub_blob);
    ssh_string(&mut blob, &private);
    let b64 = base64::engine::general_purpose::STANDARD;
    (
        format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
            b64.encode(&blob)
        ),
        format!("ssh-ed25519 {}", b64.encode(&pub_blob)),
    )
}

/// `OpenTunnelCommand._run_main`.
fn open_tunnel(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("instance-id"),
        ArgDef::new("instance-connect-endpoint-id"),
        ArgDef::new("instance-connect-endpoint-dns-name"),
        ArgDef::new("private-ip-address"),
        ArgDef::new("remote-port")
            .int()
            .default(NsVal::Int(22.into())),
        ArgDef::new("local-port").int(),
        ArgDef::new("max-tunnel-duration").int(),
        ArgDef::new("max-websocket-connections")
            .int()
            .default(NsVal::Int(10.into())),
    ];
    let Some(a) = basic::parse(ctx, &["ec2-instance-connect", "open-tunnel"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let truthy = |k: &str| a.get(k).is_some_and(Value::is_truthy);
    if !truthy("instance-id") && !truthy("private-ip-address") {
        return Err(invalid(ctx, "Specify an instance id or private ip."));
    }
    if truthy("instance-connect-endpoint-dns-name") && !truthy("instance-connect-endpoint-id") {
        return Err(invalid(
            ctx,
            "Specify an instance connect endpoint id when providing a DNS name.",
        ));
    }
    if truthy("private-ip-address") && !truthy("instance-connect-endpoint-id") {
        return Err(invalid(
            ctx,
            "Specify an instance connect endpoint id when providing a private ip.",
        ));
    }
    if let Some(d) = a.i64("max-tunnel-duration")
        && !(1..=3600).contains(&d)
    {
        return Err(invalid(
            ctx,
            "Invalid max connection timeout specified. Value must be greater than 1 and less than 3600.",
        ));
    }
    if a.get("local-port").is_none() && std::io::stdin().is_terminal() {
        return Err(invalid(
            ctx,
            "This command does not support interactive mode. You must use this command as a proxy or in listener mode. ",
        ));
    }
    let ec2 = Client::new(ctx, "ec2")?;
    let mut private_ip = a.str("private-ip-address").filter(|s| !s.is_empty());
    let (mut vpc, mut subnet) = (None, None);
    if private_ip.is_none() {
        let id = a.str("instance-id").unwrap_or_default();
        let meta = instance_metadata(ctx, &ec2, &id)?;
        vpc = Some(
            meta.get("VpcId")
                .cloned()
                .ok_or_else(|| general(ctx, "'VpcId'"))?,
        );
        subnet = Some(
            meta.get("SubnetId")
                .cloned()
                .ok_or_else(|| general(ctx, "'SubnetId'"))?,
        );
        private_ip = meta
            .get("PrivateIpAddress")
            .filter(|v| v.is_truthy())
            .or_else(|| meta.get("Ipv6Address").filter(|v| v.is_truthy()))
            .map(crate::pyfmt::py_str);
        if private_ip.is_none() {
            return Err(invalid(
                ctx,
                "Unable to find any IP address on the instance to connect to.",
            ));
        }
    }
    let mut eice_id = a
        .str("instance-connect-endpoint-id")
        .filter(|s| !s.is_empty());
    let mut dns = a
        .str("instance-connect-endpoint-dns-name")
        .filter(|s| !s.is_empty());
    if dns.is_none() {
        let eice = find_endpoint(ctx, &ec2, vpc.as_ref(), subnet.as_ref(), eice_id.as_deref())?;
        eice_id = eice
            .get("InstanceConnectEndpointId")
            .map(crate::pyfmt::py_str);
        dns = Some(eice_dns_name(ctx, &eice)?);
    }
    let dns = dns.unwrap_or_default();
    let mut query = vec![
        (
            "instanceConnectEndpointId".to_string(),
            eice_id.unwrap_or_else(|| "None".to_string()),
        ),
        (
            "remotePort".to_string(),
            a.str("remote-port").unwrap_or_default(),
        ),
        (
            "privateIpAddress".to_string(),
            private_ip.unwrap_or_default(),
        ),
    ];
    if let Some(d) = a.str("max-tunnel-duration") {
        query.push(("maxTunnelDuration".to_string(), d));
    }
    let region = presign::region(ctx)?;
    let tunnel = Tunnel {
        ctx,
        dns: dns.clone(),
        query,
        region: region.clone().unwrap_or_default(),
    };
    match a.i64("local-port") {
        None => tunnel.stdio(),
        Some(port) => tunnel.listen(port, a.i64("max-websocket-connections").unwrap_or(10)),
    }
}

/// The websocket tunnel of `WebsocketManager`.
struct Tunnel<'c> {
    ctx: &'c Context,
    dns: String,
    query: Vec<(String, String)>,
    region: String,
}

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>;

impl Tunnel<'_> {
    /// `InstanceConnectEndpointRequestSigner.get_presigned_url`.
    fn url(&self) -> Result<String, CliError> {
        let creds = presign::credentials(self.ctx, Some(&self.region))?;
        let query: String = self
            .query
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    crate::session::url::quote(k, "").replace("%20", "+"),
                    crate::session::url::quote(v, "").replace("%20", "+")
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let req = PresignRequest {
            method: "GET",
            url: format!("wss://{}/openTunnel?{query}", self.dns),
            headers: vec![("host".to_string(), self.dns.clone())],
            body: vec![],
        };
        Ok(presign_url(
            &req,
            &creds,
            "ec2-instance-connect",
            &self.region,
            60,
            Utc::now(),
        ))
    }

    fn connect(&self) -> Result<TlsStream, CliError> {
        let url = self.url()?;
        let target = url
            .split_once(&format!("wss://{}", self.dns))
            .map(|(_, t)| t.to_string())
            .unwrap_or_default();
        let fail = |e: &dyn std::fmt::Display| general(self.ctx, &e.to_string());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| fail(&e))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name =
            rustls::pki_types::ServerName::try_from(self.dns.clone()).map_err(|e| fail(&e))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| fail(&e))?;
        let tcp = std::net::TcpStream::connect((self.dns.as_str(), 443)).map_err(|e| fail(&e))?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: aws-cli/{}\r\n\r\n",
            self.dns,
            crate::model::REFERENCE_VERSION.trim()
        );
        tls.write_all(request.as_bytes()).map_err(|e| fail(&e))?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = tls.read(&mut byte).map_err(|e| fail(&e))?;
            if n == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head);
        let status = text.split_whitespace().nth(1).unwrap_or_default();
        if status != "101" {
            return Err(general(
                self.ctx,
                &format!(
                    "Failed to establish websocket connection: {}",
                    text.lines().next().unwrap_or_default()
                ),
            ));
        }
        Ok(tls)
    }

    /// Proxy stdin / stdout over one websocket.
    fn stdio(&self) -> Result<ExitCode, CliError> {
        let tls = self.connect()?;
        pump(tls, Box::new(std::io::stdin()), Box::new(std::io::stdout()));
        Ok(ExitCode::Success)
    }

    /// Listen on `port`, one websocket per accepted connection.
    fn listen(&self, port: i64, max: i64) -> Result<ExitCode, CliError> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port as u16))
            .map_err(|e| general(self.ctx, &e.to_string()))?;
        basic::print(&format!("Listening for connections on port {port}.\n"));
        let active = Arc::new(std::sync::atomic::AtomicI64::new(0));
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            if active.load(std::sync::atomic::Ordering::SeqCst) >= max {
                drop(conn);
                continue;
            }
            basic::print(&format!(
                "[{}] Accepted new tcp connection, opening websocket tunnel.\n",
                conn.peer_addr().map(|a| a.to_string()).unwrap_or_default()
            ));
            let tls = match self.connect() {
                Ok(t) => t,
                Err(e) => {
                    basic::eprint(&e.message);
                    continue;
                }
            };
            let reader = conn
                .try_clone()
                .map_err(|e| general(self.ctx, &e.to_string()))?;
            let active = active.clone();
            active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::spawn(move || {
                pump(tls, Box::new(reader), Box::new(conn));
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        Ok(ExitCode::Success)
    }
}

/// Copy `input` to the websocket as masked binary frames and the
/// websocket's frames to `output` until either side closes.
fn pump(
    tls: TlsStream,
    mut input: Box<dyn std::io::Read + Send>,
    mut output: Box<dyn std::io::Write + Send>,
) {
    let tls = Arc::new(std::sync::Mutex::new(tls));
    let tcp = tls.lock().map(|t| t.sock.try_clone().ok()).ok().flatten();
    if let Some(t) = &tcp {
        let _ = t.set_read_timeout(Some(std::time::Duration::from_millis(50)));
    }
    let writer = tls.clone();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_in = done.clone();
    let sender = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65_000];
        loop {
            let n = match input.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let frame = ws_frame(0x2, &buf[..n]);
            let Ok(mut t) = writer.lock() else { break };
            if t.write_all(&frame).is_err() {
                break;
            }
        }
        if let Ok(mut t) = writer.lock() {
            let _ = t.write_all(&ws_frame(0x8, &[]));
        }
        done_in.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16_384];
    loop {
        let read = {
            let Ok(mut t) = tls.lock() else { break };
            t.read(&mut chunk)
        };
        match read {
            Ok(0) => break,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if done.load(std::sync::atomic::Ordering::SeqCst) && pending.is_empty() {
                    break;
                }
                continue;
            }
            Err(_) => break,
        }
        let mut closed = false;
        while let Some((opcode, payload, used)) = parse_frame(&pending) {
            pending.drain(..used);
            match opcode {
                0x0..=0x2 => {
                    let _ = output.write_all(&payload);
                    let _ = output.flush();
                }
                0x8 => closed = true,
                0x9 => {
                    if let Ok(mut t) = tls.lock() {
                        let _ = t.write_all(&ws_frame(0xA, &payload));
                    }
                }
                _ => {}
            }
        }
        if closed {
            break;
        }
    }
    let _ = sender.join();
}

fn ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len < 65536 {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mask: [u8; 4] = rand::random();
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// One complete server frame: (opcode, payload, bytes used).
fn parse_frame(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = (buf[1] & 0x7f) as usize;
    let mut pos = 2;
    if len == 126 {
        if buf.len() < 4 {
            return None;
        }
        len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        pos = 4;
    } else if len == 127 {
        if buf.len() < 10 {
            return None;
        }
        len = u64::from_be_bytes(buf[2..10].try_into().ok()?) as usize;
        pos = 10;
    }
    let mask = if masked {
        if buf.len() < pos + 4 {
            return None;
        }
        let m = [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]];
        pos += 4;
        Some(m)
    } else {
        None
    };
    if buf.len() < pos + len {
        return None;
    }
    let mut payload = buf[pos..pos + len].to_vec();
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Some((opcode, payload, pos + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_and_frames() {
        assert_eq!(shell_quote("i-123"), "i-123");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
        let f = ws_frame(0x2, b"hello");
        assert_eq!(f[0], 0x82);
        let (op, payload, used) = parse_frame(&f).unwrap();
        assert_eq!((op, payload.as_slice(), used), (2, &b"hello"[..], f.len()));
    }

    #[test]
    fn key_format() {
        let (private, public) = generate_key();
        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEA"));
        assert!(public.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        if let Some(keygen) = which("ssh-keygen") {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("k");
            std::fs::write(&path, &private).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let out = std::process::Command::new(keygen)
                .args(["-y", "-f"])
                .arg(&path)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), public);
        }
    }
}
