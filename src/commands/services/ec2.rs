//! The `ec2` argument customizations: the security group shorthand
//! arguments (`awscli/customizations/ec2/secgroupsimplify.py`),
//! `get-password-data --priv-launch-key`
//! (`ec2/decryptpassword.py`), and the `bundle-instance` storage arguments
//! (`ec2/bundleinstance.py`).

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use indexmap::IndexMap;
use num_bigint::BigUint;
use sha1::Sha1;

use super::exec::{self, After};
use crate::cli::argparse::NsVal;
use crate::cli::paramfile::{expanduser, expandvars};
use crate::cli::shorthand::int_of;
use crate::cli::{Arg, ArgTable, CustomSpec, Fail, OperationHooks};
use crate::config::ConfigLookup;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::model::Operation;
use crate::pyfmt::py_str;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "authorize-security-group-ingress"
        | "authorize-security-group-egress"
        | "revoke-security-group-ingress"
        | "revoke-security-group-egress" => {
            exec::run_operation(ctx, "ec2", name, args, &mut SecurityGroup)
        }
        "bundle-instance" => {
            let binary_format = ctx
                .globals
                .cli_binary_format
                .clone()
                .or_else(|| ctx.session.config_variable("cli_binary_format"))
                .unwrap_or_else(|| "base64".to_string());
            exec::run_operation(
                ctx,
                "ec2",
                name,
                args,
                &mut BundleInstance {
                    decode_blobs: binary_format == "base64",
                },
            )
        }
        "get-password-data" => get_password_data(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// A `CustomArgument(name)` with the default keyword arguments.
pub(super) fn custom(name: &str) -> Arg<'static> {
    Arg::custom_argument(name, CustomSpec::default(), false)
}

/// Python truthiness of an argument value (`if value:`).
fn truthy(value: &Option<Value>) -> bool {
    value.as_ref().is_some_and(Value::is_truthy)
}

fn text(value: &Value) -> String {
    match value {
        Value::Str(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => py_str(other),
    }
}

/// `secgroupsimplify._add_params` / `_check_args` and the argument
/// classes' `add_to_params`.
struct SecurityGroup;

impl SecurityGroup {
    /// `_build_ip_permissions`.
    fn build(params: &mut Map, key: &str, value: Value) {
        let perms = params
            .entry("IpPermissions".to_string())
            .or_insert_with(|| Value::List(vec![Value::map()]));
        let Value::List(list) = perms else { return };
        let Some(Value::Map(first)) = list.first_mut() else {
            return;
        };
        if matches!(key, "GroupId" | "GroupName" | "UserId") {
            let pairs = first
                .entry("UserIdGroupPairs".to_string())
                .or_insert_with(|| Value::List(vec![Value::map()]));
            if let Value::List(pairs) = pairs
                && let Some(Value::Map(pair)) = pairs.first_mut()
            {
                pair.insert(key.to_string(), value);
            }
        } else {
            first.insert(key.to_string(), value);
        }
    }
}

impl OperationHooks for SecurityGroup {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        for (name, hidden) in [
            ("protocol", &["ip-protocol"][..]),
            ("port", &["from-port", "to-port"][..]),
            ("cidr", &["cidr-ip"][..]),
            ("source-group", &["source-security-group-name"][..]),
            ("group-owner", &["source-security-group-owner-id"][..]),
        ] {
            table.insert(name.to_string(), custom(name));
            for h in hidden {
                if let Some(a) = table.get_mut(*h) {
                    a.undocumented = true;
                }
            }
        }
    }

    fn args_parsed(
        &mut self,
        ns: &IndexMap<String, NsVal>,
        _paginate: &mut bool,
    ) -> Result<(), Fail> {
        if ns.get("ip_permissions").is_some_and(NsVal::is_truthy) {
            for key in ["protocol", "port", "cidr", "source_group", "group_owner"] {
                if ns.get(key).is_some_and(NsVal::is_truthy) {
                    return Err(Fail::ParamValidation(format!(
                        "The --{key} option is not compatible with the --ip-permissions option "
                    )));
                }
            }
        }
        Ok(())
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        if !truthy(&value) {
            return Ok(());
        }
        let value = value.expect("truthy");
        let s = text(&value);
        match name {
            "protocol" => {
                let mut v = value;
                match int_of(&Value::str(s.as_str())) {
                    Ok(Value::Int(i)) => {
                        let ok = (i >= 0.into() && i <= 255.into()) || i == (-1).into();
                        if !ok {
                            return Err(Fail::ParamValidation(
                                "protocol numbers must be in the range 0-255 or -1 to specify all protocols"
                                    .to_string(),
                            ));
                        }
                    }
                    _ => {
                        if !matches!(s.as_str(), "tcp" | "udp" | "icmp" | "all") {
                            return Err(Fail::ParamValidation(
                                "protocol parameter should be one of: tcp|udp|icmp|all or any valid protocol number."
                                    .to_string(),
                            ));
                        }
                        if s == "all" {
                            v = Value::str("-1");
                        }
                    }
                }
                Self::build(params, "IpProtocol", v);
            }
            "port" => {
                let (from, to) = if s == "-1" || s == "all" {
                    ("-1".to_string(), "-1".to_string())
                } else if let Some((a, b)) = s.split_once('-') {
                    (a.to_string(), b.to_string())
                } else {
                    (s.clone(), s.clone())
                };
                let error = || {
                    Fail::ParamValidation(
                        "port parameter should be of the form <from[-to]> (e.g. 22 or 22-25)"
                            .to_string(),
                    )
                };
                let from = int_of(&Value::str(from)).map_err(|_| error())?;
                Self::build(params, "FromPort", from);
                let to = int_of(&Value::str(to)).map_err(|_| error())?;
                Self::build(params, "ToPort", to);
            }
            "cidr" => {
                let mut m = Map::new();
                m.insert("CidrIp".to_string(), value);
                Self::build(params, "IpRanges", Value::List(vec![Value::Map(m)]));
            }
            "source-group" => {
                let key = if s.starts_with("sg-") {
                    "GroupId"
                } else {
                    "GroupName"
                };
                Self::build(params, key, value);
            }
            "group-owner" => Self::build(params, "UserId", value),
            _ => {}
        }
        Ok(())
    }
}

/// `bundleinstance._add_params`, `_check_args`, `_check_params`, and
/// `BundleArgument`.
struct BundleInstance {
    /// `cli_binary_format` is `base64`: `binaryformat` decodes the
    /// `UploadPolicy` string before `before-parameter-build`.
    decode_blobs: bool,
}

const BUNDLE_ARGS: &[(&str, &str)] = &[
    ("bucket", "Bucket"),
    ("prefix", "Prefix"),
    ("owner-akid", "AWSAccessKeyId"),
    ("owner-sak", "_SAK"),
    ("policy", "UploadPolicy"),
];

impl OperationHooks for BundleInstance {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        if let Some(a) = table.get_mut("storage") {
            a.required = false;
        }
        for (name, _) in BUNDLE_ARGS {
            table.insert(name.to_string(), custom(name));
        }
    }

    fn args_parsed(
        &mut self,
        ns: &IndexMap<String, NsVal>,
        _paginate: &mut bool,
    ) -> Result<(), Fail> {
        if ns.get("storage").is_some_and(NsVal::is_truthy) {
            for key in ["bucket", "prefix", "owner_akid", "owner_sak", "policy"] {
                if ns.get(key).is_some_and(NsVal::is_truthy) {
                    return Err(Fail::ParamValidation(
                        "Mixing the --storage option with the simple, scalar options is not recommended."
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn add_to_params(
        &mut self,
        name: &str,
        value: Option<Value>,
        params: &mut Map,
    ) -> Result<(), Fail> {
        if !truthy(&value) {
            return Ok(());
        }
        let Some((_, key)) = BUNDLE_ARGS.iter().find(|(n, _)| *n == name) else {
            return Ok(());
        };
        let storage = params.entry("Storage".to_string()).or_insert_with(|| {
            let mut m = Map::new();
            m.insert("S3".to_string(), Value::map());
            Value::Map(m)
        });
        if let Some(Value::Map(s3)) = storage.as_map_mut().and_then(|m| m.get_mut("S3")) {
            s3.insert(key.to_string(), value.expect("truthy"));
        }
        Ok(())
    }

    fn before_call(&mut self, params: &mut Map) -> Result<(), Fail> {
        let storage = params
            .get_mut("Storage")
            .ok_or_else(|| Fail::General("'Storage'".to_string()))?;
        let s3 = storage
            .as_map_mut()
            .and_then(|m| m.get_mut("S3"))
            .ok_or_else(|| Fail::General("'S3'".to_string()))?;
        let Value::Map(s3) = s3 else {
            return Ok(());
        };
        if self.decode_blobs
            && let Some(Value::Str(policy)) = s3.get("UploadPolicy")
        {
            let decoded = crate::protocol::common::a2b_base64(policy.as_bytes())
                .ok()
                .ok_or_else(|| Fail::General(format!("Invalid base64: \"{policy}\"")))?;
            s3.insert("UploadPolicy".to_string(), Value::Bytes(decoded));
        }
        if !s3.contains_key("UploadPolicy") {
            let expires = chrono::Utc::now() + chrono::Duration::hours(24);
            let policy = format!(
                "{{\"expiration\": \"{}\",\"conditions\": [{{\"bucket\": \"{}\"}},{{\"acl\": \"ec2-bundle-read\"}},[\"starts-with\", \"$key\", \"{}\"]]}}",
                expires.format("%Y-%m-%dT%H:%M:%S.%6fZ"),
                s3.get("Bucket").map(text).unwrap_or_default(),
                s3.get("Prefix").map(text).unwrap_or_default(),
            );
            s3.insert(
                "UploadPolicy".to_string(),
                Value::Bytes(policy.into_bytes()),
            );
        }
        if !s3.contains_key("UploadPolicySignature") {
            let policy = s3.get("UploadPolicy").filter(|v| v.is_truthy()).cloned();
            let sak = s3.get("_SAK").filter(|v| v.is_truthy()).cloned();
            if let (Some(policy), Some(sak)) = (policy, sak) {
                let Value::Bytes(policy) = policy else {
                    return Err(Fail::Python(
                        "a bytes-like object is required, not 'str'".to_string(),
                    ));
                };
                let encoded = base64::engine::general_purpose::STANDARD.encode(policy);
                let mut mac =
                    Hmac::<Sha1>::new_from_slice(text(&sak).as_bytes()).expect("hmac key");
                mac.update(encoded.as_bytes());
                let signature =
                    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
                s3.insert("UploadPolicySignature".to_string(), Value::Str(signature));
                s3.shift_remove("_SAK");
            }
        }
        Ok(())
    }
}

/// `decryptpassword.LaunchKeyArgument`.
#[derive(Default)]
struct PrivLaunchKey {
    key_path: Option<String>,
}

impl OperationHooks for PrivLaunchKey {
    fn table<'m>(&mut self, _op: &Operation<'m>, table: &mut ArgTable<'m>) {
        table.insert("priv-launch-key".to_string(), custom("priv-launch-key"));
    }

    fn add_to_params(
        &mut self,
        _name: &str,
        value: Option<Value>,
        _params: &mut Map,
    ) -> Result<(), Fail> {
        if !truthy(&value) {
            return Ok(());
        }
        let path = expanduser(&expandvars(&text(&value.expect("truthy"))));
        if std::path::Path::new(&path).is_file() {
            self.key_path = Some(path);
            Ok(())
        } else {
            Err(Fail::Python(
                "priv-launch-key should be a path to the local SSH private key file used to launch the instance."
                    .to_string(),
            ))
        }
    }
}

fn get_password_data(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut hooks = PrivLaunchKey::default();
    let Some(inv) = exec::parse_operation(ctx, "ec2", "get-password-data", args, &mut hooks)?
    else {
        return Ok(ExitCode::Success);
    };
    let key_path = hooks.key_path;
    exec::invoke(ctx, inv, &mut |parsed| {
        let Some(path) = &key_path else {
            return Ok(After::Display);
        };
        let Some(Value::Map(m)) = Some(&mut *parsed) else {
            return Ok(After::Display);
        };
        let Some(value) = m.get("PasswordData").filter(|v| v.is_truthy()) else {
            return Ok(After::Display);
        };
        let decrypted = decrypt_password(path, &text(value)).ok_or_else(|| {
            ctx.general_error(&anyhow::anyhow!(
                "Unable to decrypt password data using provided private key file."
            ))
        })?;
        m.insert("PasswordData".to_string(), Value::Str(decrypted));
        Ok(After::Display)
    })
}

/// `_decrypt_password_data`: PKCS #1 v1.5 decryption with the PEM private
/// key at `path`.
fn decrypt_password(path: &str, value: &str) -> Option<String> {
    let pem = std::fs::read_to_string(path).ok()?;
    let (n, d) = rsa_private_key(&pem)?;
    let cipher = crate::protocol::common::a2b_base64(value.as_bytes()).ok()?;
    let k = n.to_bytes_be().len();
    let c = BigUint::from_bytes_be(&cipher);
    if c >= n {
        return None;
    }
    let m = c.modpow(&d, &n).to_bytes_be();
    let mut em = vec![0u8; k.saturating_sub(m.len())];
    em.extend_from_slice(&m);
    if em.len() < 11 || em[0] != 0 || em[1] != 2 {
        return None;
    }
    let sep = em[2..].iter().position(|&b| b == 0)? + 2;
    if sep < 10 {
        return None;
    }
    String::from_utf8(em[sep + 1..].to_vec()).ok()
}

/// The modulus and private exponent of a PEM RSA private key (PKCS #1
/// `RSA PRIVATE KEY` or PKCS #8 `PRIVATE KEY`).
fn rsa_private_key(pem: &str) -> Option<(BigUint, BigUint)> {
    let mut body = String::new();
    let mut inside = false;
    let mut pkcs8 = false;
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            inside = true;
            pkcs8 = !line.contains("RSA");
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if inside && !line.contains(':') {
            body.push_str(line);
        }
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    let mut key = der.as_slice();
    if pkcs8 {
        let (_, seq) = der_read(der.as_slice(), 0x30)?;
        let (rest, _) = der_read(seq, 0x02)?;
        let (rest, _) = der_read(rest, 0x30)?;
        let (_, octets) = der_read(rest, 0x04)?;
        key = octets;
    }
    let (_, seq) = der_read(key, 0x30)?;
    let (rest, _version) = der_read(seq, 0x02)?;
    let (rest, n) = der_read(rest, 0x02)?;
    let (rest, _e) = der_read(rest, 0x02)?;
    let (_, d) = der_read(rest, 0x02)?;
    Some((BigUint::from_bytes_be(n), BigUint::from_bytes_be(d)))
}

/// Read one DER element with `tag`: (the rest, its contents).
fn der_read(data: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    if *data.first()? != tag {
        return None;
    }
    let first = *data.get(1)?;
    let (len, start) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *data.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = start.checked_add(len)?;
    Some((data.get(end..)?, data.get(start..end)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_group_params() {
        let mut params = Map::new();
        let mut h = SecurityGroup;
        h.add_to_params("protocol", Some(Value::str("all")), &mut params)
            .unwrap();
        h.add_to_params("port", Some(Value::str("22-25")), &mut params)
            .unwrap();
        h.add_to_params("cidr", Some(Value::str("10.0.0.0/8")), &mut params)
            .unwrap();
        h.add_to_params("source-group", Some(Value::str("sg-1")), &mut params)
            .unwrap();
        let text = crate::pyfmt::json_dumps_pretty(&Value::Map(params));
        assert!(text.contains("\"IpProtocol\": \"-1\""));
        assert!(text.contains("\"FromPort\": 22"));
        assert!(text.contains("\"GroupId\": \"sg-1\""));
        let e = h
            .add_to_params("port", Some(Value::str("a")), &mut Map::new())
            .unwrap_err();
        assert!(matches!(e, Fail::ParamValidation(_)));
        let e = h
            .add_to_params("protocol", Some(Value::str("300")), &mut Map::new())
            .unwrap_err();
        assert!(matches!(e, Fail::ParamValidation(m) if m.starts_with("protocol numbers")));
    }

    #[test]
    fn decrypts_pkcs1_v15() {
        let dir = tempfile::tempdir().unwrap();
        for (i, pem) in [PEM1, PEM8].iter().enumerate() {
            let path = dir.path().join(format!("k{i}.pem"));
            std::fs::write(
                &path,
                base64::engine::general_purpose::STANDARD
                    .decode(pem)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                decrypt_password(path.to_str().unwrap(), CIPHER).as_deref(),
                Some("P@ssw0rd!")
            );
        }
    }

    const CIPHER: &str = "mJZVpIpMwVmyw7DFzt43qlKGio9STVVSSqfY0r6XfOd9mUh/6llJvjkr2FXwtSipHlo1m9Rh0hQ0Vyp5XKdR75T+ry3F4UvozmZCAEo2MNE7npQDlHQRPouIa1RFOnUFYbZv4jz2P10oQQqBt4j8ruYoXriIWRYoexNgHs/VL+w=";
    /// Base64 of the PEM files.
    const PEM1: &str = "LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQpNSUlDWEFJQkFBS0JnUUN5Q2xyWE1sMUE4RldRQ28wVHprV3JxWmVMaHFRQU5DbkVLNkQvV21iVU9Hak1FYzdZCnNFbXR0Vk04Uko1MU9jUmY1aVl3Sm1PbUdSNkw3S0RNVldkcWZRdkQzOXRXZGFZcUg0blJ5MHlnTkgyNmQwM1MKVTNQcWljVHNjNHdIT0E3V3RsWENoK0dreGdVMWdOWkxNWWNGZUZRNm1FUkNES2tMTnZqSDVFZzZBUUlEQVFBQgpBb0dBTXJlS2F4VDR4Wi93NzhoaHFjdXZ3eGZiaUs2QkYySEVxQkk1bWFRNStLaGRDZ3prbkl1VFlYMXV5Wk5PCmhGeEJXWVFwU3J3VTFPTUE5YUVocVRPRzhyQUs4T2p4Sk44QlNNZ29mc0NjVEJld2hGeFpVQUpNTkZvR2c2Z2IKRDdKYWo3RGNOc0ZDeHpGNC84ckMrWnc4dXB5NzNvUHptdlVCT3JFNHlWV1Jyc0VDUVFEZm53d1NvcmJ4ZnFnTgpCMDR6WUR6MUhHT25YTnR1ZUhia1I0REZjYUh0blVyajZ0eFNhTmpwMlprVHZRZEtJVm5yeVc1b1ZsdUUxeEUvCkk4RVBoSHNKQWtFQXk5SEVQWlROTkZoVDZPTnNDS2RjMkErMUhZOFVSYXFwZmhoMWxmV0ordDJ1MHdrejFFaVgKSlQzMGI4MmlwQ0hna2JTSkJvZUdFV1ZaZjFWTTE3WnRPUUpCQUpkQnRGL1NrZDN0WnV4UkdiTWtrSGJXK2cwVgpnUk5MQmxoSWlUQ0VPUnhKUFlQR3M3cEdVeHB5WmRsMURiNDFxdVlTL013MFJYa29JL0ZLVk9jVEhFa0NRRkpmCjBoY3RaNDRMVU1aais4cTNrVytGVWFiRHArYk1KYnBmdWs1RU1XTGhhNEhCUzJBMWdsaTc1R20zUzA5WW5jRmwKbktlMTZEcHBvclV5elY2MVRRa0NRQmI4eVE1MjJmc0JFZkZSNTArQnc4akI4MjltV2VFY1l4aXgvb2l0LzVvMQpmSjBNWExZWGp3TFkrZGdVU2F4eU5jVy9ZWTBHNkQyUFNuMXVwQTMwOG9nPQotLS0tLUVORCBSU0EgUFJJVkFURSBLRVktLS0tLQo="; // gitleaks:allow
    const PEM8: &str = "LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0tCk1JSUNkZ0lCQURBTkJna3Foa2lHOXcwQkFRRUZBQVNDQW1Bd2dnSmNBZ0VBQW9HQkFMSUtXdGN5WFVEd1ZaQUsKalJQT1JhdXBsNHVHcEFBMEtjUXJvUDlhWnRRNGFNd1J6dGl3U2EyMVV6eEVublU1eEYvbUpqQW1ZNllaSG92cwpvTXhWWjJwOUM4UGYyMVoxcGlvZmlkSExUS0EwZmJwM1RkSlRjK3FKeE94empBYzREdGEyVmNLSDRhVEdCVFdBCjFrc3hod1Y0VkRxWVJFSU1xUXMyK01ma1NEb0JBZ01CQUFFQ2dZQXl0NHByRlBqRm4vRHZ5R0dweTYvREY5dUkKcm9FWFljU29Fam1acERuNHFGMEtET1NjaTVOaGZXN0prMDZFWEVGWmhDbEt2QlRVNHdEMW9TR3BNNGJ5c0Fydwo2UEVrM3dGSXlDaCt3SnhNRjdDRVhGbFFBa3cwV2dhRHFCc1BzbHFQc053MndVTEhNWGoveXNMNW5EeTZuTHZlCmcvT2E5UUU2c1RqSlZaR3V3UUpCQU4rZkRCS2l0dkYrcUEwSFRqTmdQUFVjWTZkYzIyNTRkdVJIZ01WeG9lMmQKU3VQcTNGSm8yT25abVJPOUIwb2hXZXZKYm1oV1c0VFhFVDhqd1ErRWV3a0NRUURMMGNROWxNMDBXRlBvNDJ3SQpwMXpZRDdVZGp4UkZxcWwrR0hXVjlZbjYzYTdUQ1RQVVNKY2xQZlJ2emFLa0llQ1J0SWtHaDRZUlpWbC9WVXpYCnRtMDVBa0VBbDBHMFg5S1IzZTFtN0ZFWnN5U1FkdGI2RFJXQkUwc0dXRWlKTUlRNUhFazlnOGF6dWtaVEduSmwKMlhVTnZqV3E1aEw4ekRSRmVTZ2o4VXBVNXhNY1NRSkFVbC9TRnkxbmpndFF4bVA3eXJlUmI0VlJwc09uNXN3bAp1bCs2VGtReFl1RnJnY0ZMWURXQ1dMdmthYmRMVDFpZHdXV2NwN1hvT21taXRUTE5YclZOQ1FKQUZ2ekpEbmJaCit3RVI4VkhuVDRIRHlNSHpiMlpaNFJ4akdMSCtpSzMvbWpWOG5ReGN0aGVQQXRqNTJCUkpySEkxeGI5aGpRYm8KUFk5S2ZXNmtEZlR5aUE9PQotLS0tLUVORCBQUklWQVRFIEtFWS0tLS0tCg=="; // gitleaks:allow
}
