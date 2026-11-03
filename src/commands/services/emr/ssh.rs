//! `emr ssh`, `socks`, `get`, `put` (`awscli/customizations/emr/ssh.py`,
//! `sshutils.py`).

use super::super::basic::{self, Args};
use super::super::exec::Client;
use super::util::{self, general_error};
use crate::driver::{CallResult, Context};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

const TERMINATED_STATES: &[&str] = &["TERMINATED", "TERMINATING", "TERMINATED_WITH_ERRORS"];
const STARTING_STATES: &[&str] = &["STARTING", "BOOTSTRAPPING"];
const UNSUPPORTED_OPTION_MSG: &str = "WARNING: Your OpenSSH version does not support StrictHostKeyChecking=accept-new (requires OpenSSH 7.6+). Falling back to StrictHostKeyChecking=no. Upgrade to OpenSSH 7.6+ for improved security.\n";
const PUTTY_SSH_OPTIONS_MSG: &str = "WARNING: --ssh-options is only supported with OpenSSH. Options are ignored when using PuTTY/pscp.\n";

/// `emrutils.which`.
fn which(program: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let dir = dir.trim_matches('"');
        let exe = std::path::Path::new(dir).join(program);
        if let Ok(meta) = std::fs::metadata(&exe)
            && meta.is_file()
            && nix_access(&exe)
        {
            return Some(exe.display().to_string());
        }
    }
    None
}

fn nix_access(path: &std::path::Path) -> bool {
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

fn describe(client: &Client<'_>, cluster_id: &str) -> Result<Value, CliError> {
    let mut p = Map::new();
    p.insert("ClusterId".to_string(), Value::str(cluster_id));
    client.call("DescribeCluster", &Value::Map(p))
}

/// `sshutils.validate_and_find_master_dns`.
fn master_dns(ctx: &Context, cluster_id: &str) -> Result<String, CliError> {
    let client = util::emr_client(ctx)?;
    let state = describe(&client, cluster_id)?
        .get("Cluster")
        .and_then(|c| c.get("Status"))
        .and_then(|s| s.get("State"))
        .map(crate::pyfmt::py_str)
        .unwrap_or_default();
    if TERMINATED_STATES.contains(&state.as_str()) {
        return Err(general_error(
            ctx,
            "Cluster terminating or already terminated.",
        ));
    }
    if STARTING_STATES.contains(&state.as_str()) {
        basic::print("Waiting for the cluster to start.\n");
    }
    let config = client
        .model
        .waiters()
        .and_then(|w| w.get("ClusterRunning"))
        .and_then(|w| w.as_object())
        .ok_or_else(|| general_error(ctx, "Waiter does not exist: ClusterRunning"))?;
    let mut params = Map::new();
    params.insert("ClusterId".to_string(), Value::str(cluster_id));
    let mut poll = |p: &Value| -> anyhow::Result<Value> {
        match client.try_call("DescribeCluster", p) {
            Ok(CallResult::Ok(v)) => Ok(v),
            Ok(CallResult::ServiceError(info)) => Ok(info.parsed),
            Err(e) => Err(anyhow::anyhow!("{}", e.message)),
        }
    };
    let outcome = crate::driver::block_on(
        &ctx.runtime,
        crate::waiter::wait("ClusterRunning", config, &Value::Map(params), &mut poll),
    )
    .map_err(|e| ctx.general_error(&e))?;
    if outcome.stderr().is_some() {
        return Err(general_error(
            ctx,
            "Cannot get DNS of master node on the cluster.  Please try again after some time.",
        ));
    }
    let cluster = describe(&client, cluster_id)?;
    match cluster
        .get("Cluster")
        .and_then(|c| c.get("MasterPublicDnsName"))
    {
        Some(v) => Ok(crate::pyfmt::py_str(v)),
        None => Err(general_error(ctx, "'MasterPublicDnsName'")),
    }
}

/// `_supports_accept_new`.
fn supports_accept_new() -> bool {
    std::process::Command::new("ssh")
        .args(["-G", "-o", "StrictHostKeyChecking=accept-new", "localhost"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `_build_ssh_options`.
fn ssh_options(extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let has_override = extra
        .iter()
        .any(|o| o.to_lowercase().starts_with("stricthostkeychecking="));
    if !has_override {
        if supports_accept_new() {
            out.extend([
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
            ]);
        } else {
            basic::eprint(UNSUPPORTED_OPTION_MSG);
            out.extend(["-o".to_string(), "StrictHostKeyChecking=no".to_string()]);
        }
    }
    for o in extra {
        out.extend(["-o".to_string(), o.clone()]);
    }
    out
}

/// `check_command_key_format` with `['ppk']`.
fn is_ppk(key: &str) -> bool {
    key.ends_with("ppk")
}

pub(crate) fn run(ctx: &Context, name: &str, parsed: &Args) -> Result<ExitCode, CliError> {
    let cluster_id = parsed.str("cluster_id").unwrap_or_default();
    let key = parsed.str("key_pair_file").unwrap_or_default();
    let extra = parsed.str_list("ssh_options");
    let dns = master_dns(ctx, &cluster_id)?;
    let host = format!("hadoop@{dns}");
    let have = |p: &str| which(p).is_some();
    let mut temp: Option<std::path::PathBuf> = None;
    let command: Vec<String> = match name {
        "ssh" | "socks" => {
            if !(have("putty.exe") || have("ssh") || have("ssh.exe")) {
                return Err(general_error(
                    ctx,
                    "SSH or Putty not available. Please refer to the documentation at http://docs.aws.amazon.com/ElasticMapReduce/latest/DeveloperGuide/EMR_SetUp_SSH.html.",
                ));
            }
            if have("putty.exe") && !(have("ssh.exe") || have("ssh")) && !is_ppk(&key) {
                return Err(putty_key_error(ctx));
            }
            temp = Some(named_temporary_file());
            if have("ssh") || have("ssh.exe") {
                let mut c = vec!["ssh".to_string()];
                c.extend(ssh_options(extra.as_deref().unwrap_or_default()));
                c.extend(["-o".to_string(), "ServerAliveInterval=10".to_string()]);
                if name == "socks" {
                    c.extend(["-ND".to_string(), "8157".to_string()]);
                }
                c.extend(["-i".to_string(), key.clone(), host]);
                if name == "ssh" {
                    c.push("-t".to_string());
                    if let Some(cmd) = parsed.str("command").filter(|c| !c.is_empty()) {
                        c.push(cmd);
                    }
                }
                c
            } else {
                if extra.as_ref().is_some_and(|e| !e.is_empty()) {
                    basic::eprint(PUTTY_SSH_OPTIONS_MSG);
                }
                let mut c = vec![
                    "putty".to_string(),
                    "-ssh".to_string(),
                    "-i".to_string(),
                    key.clone(),
                    host,
                ];
                if name == "socks" {
                    c.extend(["-N".to_string(), "-D".to_string(), "8157".to_string()]);
                } else {
                    c.push("-t".to_string());
                    if let Some(cmd) = parsed.str("command").filter(|c| !c.is_empty())
                        && let Some(path) = &temp
                    {
                        let _ = std::fs::write(
                            path,
                            format!("{cmd}\nread -n1 -r -p \"Command completed. Press any key.\""),
                        );
                        c.push("-m".to_string());
                        c.push(path.display().to_string());
                    }
                }
                c
            }
        }
        _ => {
            if !(have("pscp.exe") || have("scp") || have("scp.exe")) {
                return Err(general_error(
                    ctx,
                    "SCP or Pscp not available. Please refer to the documentation at http://docs.aws.amazon.com/ElasticMapReduce/latest/DeveloperGuide/EMR_SetUp_SSH.html. ",
                ));
            }
            if have("pscp.exe") && !(have("scp.exe") || have("scp")) && !is_ppk(&key) {
                return Err(putty_key_error(ctx));
            }
            let src = parsed.str("src").unwrap_or_default();
            let dest = parsed.str("dest").filter(|d| !d.is_empty());
            let base = src.rsplit('/').next().unwrap_or_default().to_string();
            let mut c = if have("scp") || have("scp.exe") {
                let mut c = vec!["scp".to_string(), "-r".to_string()];
                c.extend(ssh_options(extra.as_deref().unwrap_or_default()));
                c.extend(["-i".to_string(), key.clone()]);
                c
            } else {
                if extra.as_ref().is_some_and(|e| !e.is_empty()) {
                    basic::eprint(PUTTY_SSH_OPTIONS_MSG);
                }
                vec![
                    "pscp".to_string(),
                    "-scp".to_string(),
                    "-r".to_string(),
                    "-i".to_string(),
                    key.clone(),
                ]
            };
            if name == "put" {
                c.push(src.clone());
                c.push(format!("{host}:{}", dest.unwrap_or(base)));
            } else {
                c.push(format!("{host}:{src}"));
                c.push(dest.unwrap_or(base));
            }
            c
        }
    };
    basic::print(&format!("{}\n", command.join(" ")));
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status();
    if name == "ssh"
        && let Some(path) = &temp
    {
        let _ = std::fs::remove_file(path);
    }
    let rc = match status {
        Ok(s) => s.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            -s.signal().unwrap_or(1)
        }),
        Err(e) => {
            return Err(general_error(
                ctx,
                &crate::cli::paramfile::os_error(&e, &command[0]),
            ));
        }
    };
    exit_with(rc)
}

fn putty_key_error(ctx: &Context) -> CliError {
    util::param_error(
        ctx,
        "Key file file format is incorrect. Putty expects a ppk file. Please refer to documentation at http://docs.aws.amazon.com/ElasticMapReduce/latest/DeveloperGuide/EMR_SetUp_SSH.html. ",
    )
}

/// `tempfile.NamedTemporaryFile(delete=False)`.
fn named_temporary_file() -> std::path::PathBuf {
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let name: String = (0..8)
        .map(|_| {
            let chars = b"abcdefghijklmnopqrstuvwxyz0123456789_";
            chars[rand::random_range(0..chars.len())] as char
        })
        .collect();
    let path = std::path::Path::new(&dir).join(format!("tmp{name}"));
    let _ = std::fs::File::create(&path);
    path
}

/// The command's return code as the process exit status.
fn exit_with(rc: i32) -> Result<ExitCode, CliError> {
    Ok(match rc {
        0 => ExitCode::Success,
        1 => ExitCode::S3TransferFailed,
        2 => ExitCode::S3Skipped,
        130 => ExitCode::Interrupted,
        252 => ExitCode::Usage,
        253 => ExitCode::Config,
        254 => ExitCode::Service,
        255 => ExitCode::General,
        other => {
            basic::flush();
            std::process::exit(other.rem_euclid(256));
        }
    })
}
