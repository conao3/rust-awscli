//! Running one CLI invocation in an isolated environment, optionally with
//! stdout on a pseudo terminal of a given width.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Env {
    pub max_attempts: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RunOut {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub outfile: Option<Vec<u8>>,
}

const SSO_SESSION: &str = "difftest";
const CONFIG: &str = "[default]\n\
region = us-east-1\n\
sso_session = difftest\n\
\n\
[sso-session difftest]\n\
sso_region = us-east-1\n\
sso_start_url = https://difftest.awsapps.com/start\n";
/// A cached SSO token for bearer-auth services (botocore
/// `SSOTokenLoader`), valid far into the future so it is never refreshed.
const SSO_TOKEN: &str = r#"{"startUrl": "https://difftest.awsapps.com/start", "region": "us-east-1", "accessToken": "difftest-bearer-token", "expiresAt": "2099-01-01T00:00:00Z"}"#;
/// The static fake credentials of the SigV4 test suite.
const CREDENTIALS: &str = "[default]\n\
aws_access_key_id = AKIDEXAMPLE\n\
aws_secret_access_key = difftest-fake-secret-key\n";

/// Create `dir/home/.aws/{config,credentials}`.
pub fn write_home(dir: &Path) -> Result<PathBuf> {
    let home = dir.join("home");
    let aws = home.join(".aws");
    std::fs::create_dir_all(&aws)?;
    std::fs::write(aws.join("config"), CONFIG)?;
    std::fs::write(aws.join("credentials"), CREDENTIALS)?;
    let cache = aws.join("sso/cache");
    std::fs::create_dir_all(&cache)?;
    let key = hex::encode(Sha1::digest(SSO_SESSION.as_bytes()));
    std::fs::write(cache.join(format!("{key}.json")), SSO_TOKEN)?;
    Ok(home)
}

pub async fn run(
    bin: &Path,
    argv: &[String],
    dir: &Path,
    env: &Env,
    pty: Option<u16>,
    outfile: Option<&str>,
) -> Result<RunOut> {
    let home = dir.join("home");
    if let Some(f) = outfile {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let mut cmd = Command::new(bin);
    cmd.args(argv)
        .current_dir(dir)
        .env_clear()
        .envs(child_env(&home, env))
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut pty_reader = None;
    if let Some(cols) = pty {
        let (master, slave) = open_pty(cols)?;
        cmd.stdout(Stdio::from(slave));
        pty_reader = Some(master);
    } else {
        cmd.stdout(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    // Close the parent's copy of the pty slave so reads end at EOF.
    drop(cmd);

    let stdout_task = match pty_reader {
        Some(master) => tokio::task::spawn_blocking(move || {
            let mut f = std::fs::File::from(master);
            let mut buf = vec![];
            let mut chunk = [0u8; 8192];
            loop {
                match f.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            buf
        }),
        None => {
            let mut out = child.stdout.take().unwrap();
            tokio::spawn(async move {
                let mut buf = vec![];
                let _ = out.read_to_end(&mut buf).await;
                buf
            })
        }
    };
    let mut err = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut buf = vec![];
        let _ = err.read_to_end(&mut buf).await;
        buf
    });

    let pgid = child.id().map(|id| nix::unistd::Pid::from_raw(id as i32));
    let mut result = RunOut::default();
    match tokio::time::timeout(env.timeout, child.wait()).await {
        Ok(status) => result.code = status?.code(),
        Err(_) => {
            result.timed_out = true;
            let _ = child.kill().await;
        }
    }
    // Helpers the CLI started (session-manager-plugin) would keep the
    // output pipes open.
    if let Some(pgid) = pgid {
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    }
    let drain = Duration::from_secs(5);
    result.stdout = match tokio::time::timeout(drain, stdout_task).await {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => vec![],
    };
    result.stderr = match tokio::time::timeout(drain, stderr_task).await {
        Ok(r) => r.unwrap_or_default(),
        Err(_) => vec![],
    };
    if let Some(f) = outfile {
        result.outfile = std::fs::read(dir.join(f)).ok();
    }
    Ok(result)
}

/// The whole environment of a CLI run with `home` as its home directory.
pub fn child_env(home: &Path, env: &Env) -> Vec<(&'static str, std::ffi::OsString)> {
    vec![
        ("PATH", child_path()),
        ("HOME", home.into()),
        ("AWS_CONFIG_FILE", home.join(".aws/config").into()),
        (
            "AWS_SHARED_CREDENTIALS_FILE",
            home.join(".aws/credentials").into(),
        ),
        ("AWS_PAGER", "".into()),
        ("LC_ALL", "C.UTF-8".into()),
        ("TZ", "UTC".into()),
        ("AWS_EC2_METADATA_DISABLED", "true".into()),
        ("AWS_MAX_ATTEMPTS", env.max_attempts.to_string().into()),
        ("NO_PROXY", "*".into()),
        ("no_proxy", "*".into()),
    ]
}

/// Open a pty pair of `cols` columns with output post-processing off, so
/// the bytes read from the master are exactly what the child wrote.
pub fn open_pty(cols: u16) -> Result<(OwnedFd, OwnedFd)> {
    use nix::pty::{Winsize, openpty};
    use nix::sys::termios::{OutputFlags, SetArg, tcgetattr, tcsetattr};
    let ws = Winsize {
        ws_row: 50,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pair = openpty(Some(&ws), None).context("openpty")?;
    // Children spawned concurrently for other cases must not inherit the
    // pair, or this case's reader would not see EOF when its child exits.
    for fd in [&pair.master, &pair.slave] {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).context("fcntl")?;
    }
    let mut t = tcgetattr(&pair.slave).context("tcgetattr")?;
    t.output_flags.remove(OutputFlags::OPOST);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &t).context("tcsetattr")?;
    Ok((pair.master, pair.slave))
}

/// `PATH` for the CLIs under test: `difftest/fakebin` (stand-ins for
/// `session-manager-plugin` and `ssh`) first, then the harness's `PATH`.
fn child_path() -> std::ffi::OsString {
    let fakebin = Path::new(env!("CARGO_MANIFEST_DIR")).join("fakebin");
    let mut paths = vec![fakebin];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(paths).unwrap_or_default()
}
