//! Running the reference CLI through a fork server
//! (`difftest/refserver/awsr_refserver.py`): one reference process that
//! has imported the CLI forks a fresh run for every case, which saves the
//! interpreter start and the imports that dominate a reference run.

use std::io::{IoSlice, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::fcntl::OFlag;
use nix::sys::signal::{Signal, killpg};
use nix::sys::socket::{ControlMessage, MsgFlags, UnixAddr, sendmsg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::run::{self, Env, RunOut};

#[derive(Debug)]
pub struct RefServer {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    _child: Child,
}

impl RefServer {
    pub async fn start(reference: &Path, work: &Path, env: &Env) -> Result<RefServer> {
        std::fs::create_dir_all(work)?;
        let dir = tempfile::Builder::new()
            .prefix("refserver-")
            .tempdir_in(work)?;
        let home = run::write_home(dir.path())?;
        let plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("refserver");
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(home.join(".aws/config"))?;
        write!(
            config,
            "\n[plugins]\ncli_legacy_plugin_path = {}\nrefserver = awsr_refserver\n",
            plugin_dir.display()
        )?;
        let socket = dir.path().join("socket");
        let mut child = Command::new(reference)
            .args(["sts", "get-caller-identity"])
            .current_dir(dir.path())
            .env_clear()
            .envs(run::child_env(&home, env))
            .env("AWSR_REFSERVER_SOCKET", &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", reference.display()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        let ready = tokio::time::timeout(
            Duration::from_secs(60),
            BufReader::new(stdout).read_line(&mut line),
        )
        .await;
        if !matches!(ready, Ok(Ok(_))) || line != "ready\n" {
            let _ = child.kill().await;
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = e.read_to_string(&mut err).await;
            }
            bail!("reference fork server did not start: {line:?}\n{err}");
        }
        Ok(RefServer {
            socket,
            _dir: dir,
            _child: child,
        })
    }

    /// [`run::run`] of the reference CLI, forked from the server.
    pub async fn run(
        &self,
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
        let (out_r, out_w) = match pty {
            Some(cols) => run::open_pty(cols)?,
            None => nix::unistd::pipe2(OFlag::O_CLOEXEC)?,
        };
        let (err_r, err_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let null = std::fs::File::open("/dev/null")?;
        let request = serde_json::json!({
            "argv": argv,
            "env": run::child_env(&home, env)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string_lossy().into_owned()))
                .collect::<std::collections::BTreeMap<_, _>>(),
            "cwd": dir,
        })
        .to_string();
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)
            .context("connect to the reference fork server")?;
        let header = (request.len() as u64).to_be_bytes();
        let fds = [null.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()];
        sendmsg::<UnixAddr>(
            stream.as_raw_fd(),
            &[IoSlice::new(&header)],
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None,
        )
        .context("send the case to the reference fork server")?;
        stream.write_all(request.as_bytes())?;
        drop((out_w, err_w, null));

        let stdout_task = tokio::task::spawn_blocking(move || read_all(out_r));
        let stderr_task = tokio::task::spawn_blocking(move || read_all(err_r));
        stream.set_nonblocking(true)?;
        let mut lines = BufReader::new(tokio::net::UnixStream::from_std(stream)?).lines();
        let pid = match lines.next_line().await? {
            Some(l) => l
                .strip_prefix("pid ")
                .and_then(|p| p.parse::<i32>().ok())
                .with_context(|| format!("fork server reply {l:?}"))?,
            None => bail!("the reference fork server closed the connection"),
        };
        let pgid = Pid::from_raw(pid);
        let mut result = RunOut::default();
        match tokio::time::timeout(env.timeout, lines.next_line()).await {
            Ok(Ok(Some(l))) => {
                result.code = match l.split_once(' ') {
                    Some(("exit", c)) => Some(c.parse()?),
                    Some(("signal", _)) => None,
                    _ => bail!("fork server reply {l:?}"),
                }
            }
            Ok(Ok(None)) => bail!("the reference fork server closed the connection"),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => result.timed_out = true,
        }
        let _ = killpg(pgid, Signal::SIGKILL);
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
}

fn read_all(fd: OwnedFd) -> Vec<u8> {
    let mut f = std::fs::File::from(fd);
    let mut buf = vec![];
    let mut chunk = [0u8; 8192];
    loop {
        match f.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}
