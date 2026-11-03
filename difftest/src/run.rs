//! Running one CLI invocation in an isolated environment, optionally with
//! stdout on a pseudo terminal of a given width.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Env {
    pub max_attempts: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct RunOut {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub outfile: Option<Vec<u8>>,
}

const CONFIG: &str = "[default]\nregion = us-east-1\n";
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
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("AWS_CONFIG_FILE", home.join(".aws/config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", home.join(".aws/credentials"))
        .env("AWS_PAGER", "")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_MAX_ATTEMPTS", env.max_attempts.to_string())
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
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

    let mut result = RunOut::default();
    match tokio::time::timeout(env.timeout, child.wait()).await {
        Ok(status) => result.code = status?.code(),
        Err(_) => {
            result.timed_out = true;
            let _ = child.kill().await;
        }
    }
    result.stdout = stdout_task.await.unwrap_or_default();
    result.stderr = stderr_task.await.unwrap_or_default();
    if let Some(f) = outfile {
        result.outfile = std::fs::read(dir.join(f)).ok();
    }
    Ok(result)
}

/// Open a pty pair of `cols` columns with output post-processing off, so
/// the bytes read from the master are exactly what the child wrote.
fn open_pty(cols: u16) -> Result<(OwnedFd, OwnedFd)> {
    use nix::pty::{Winsize, openpty};
    use nix::sys::termios::{OutputFlags, SetArg, tcgetattr, tcsetattr};
    let ws = Winsize {
        ws_row: 50,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pair = openpty(Some(&ws), None).context("openpty")?;
    let mut t = tcgetattr(&pair.slave).context("tcgetattr")?;
    t.output_flags.remove(OutputFlags::OPOST);
    tcsetattr(&pair.slave, SetArg::TCSANOW, &t).context("tcsetattr")?;
    Ok((pair.master, pair.slave))
}
