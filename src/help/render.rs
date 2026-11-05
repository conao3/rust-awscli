//! `awscli/help.py` `PosixHelpRenderer` / `PosixPagingHelpRenderer`:
//! the document through docutils' manpage writer and groff (or mandoc),
//! then the pager.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use super::HelpRequest;

/// `ExecutableNotFoundError`.
pub(crate) fn executable_not_found(name: &str) -> String {
    format!("Could not find executable named \"{name}\"")
}

/// `PosixPagingHelpRenderer._exists_on_path`.
fn exists_on_path(name: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').any(|p| {
        if p.is_empty() || name.starts_with('/') {
            Path::new(name).exists()
        } else {
            Path::new(p).join(name).exists()
        }
    })
}

/// `shlex.split` (POSIX mode, no comments).
fn shlex_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '\\' => {
                in_token = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' => {
                in_token = true;
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    cur.push(n);
                }
            }
            '"' => {
                in_token = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('\\' | '"')) => {
                            cur.push(chars.next().expect("peeked"));
                        }
                        _ => cur.push(n),
                    }
                }
            }
            _ => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

/// Run `cmdline` with `input` on stdin, returning its stdout
/// (`Popen.communicate`).
fn communicate(cmdline: &[&str], input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut child = Command::new(cmdline[0])
        .args(&cmdline[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stderr = child.stderr.take().expect("piped");
    let drain = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });
    let mut out = Vec::new();
    child.stdout.take().expect("piped").read_to_end(&mut out)?;
    let _ = writer.join();
    let _ = drain.join();
    child.wait()?;
    Ok(out)
}

/// `PosixHelpRenderer._convert_doc_content` (`device` `ascii`) and
/// `PosixBrowserHelpRenderer._convert_doc_content` (`html`) after the
/// manpage writer.
pub(crate) fn groff(man: &[u8], device: &str) -> Result<Vec<u8>, String> {
    let cmdline: Vec<&str> = match formatter() {
        Some("groff") => vec!["groff", "-m", "man", "-T", device],
        Some(_) => vec!["mandoc", "-T", device],
        None => return Err(executable_not_found("groff or mandoc")),
    };
    communicate(&cmdline, man).map_err(|e| e.to_string())
}

pub(crate) fn formatter() -> Option<&'static str> {
    ["groff", "mandoc"].into_iter().find(|p| exists_on_path(p))
}

pub(crate) fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(|p| {
            if p.is_empty() || name.starts_with('/') {
                std::path::PathBuf::from(name)
            } else {
                Path::new(p).join(name)
            }
        })
        .find(|p| p.exists())
}

/// `BrowserHelpRenderer._send_output_to_browser`: the page in a temporary
/// `.html` file opened with the default browser.
pub(crate) fn open_in_browser(html: &[u8]) -> Result<(), String> {
    let mut file = tempfile_html().map_err(|e| e.to_string())?;
    file.1.write_all(html).map_err(|e| e.to_string())?;
    println!("Opening help file in the default browser.");
    let url = format!("file://{}", file.0.display());
    let browser = std::env::var("BROWSER").ok().filter(|b| !b.is_empty());
    let cmdline = match browser {
        Some(b) if b.contains("%s") => shlex_split(&b.replace("%s", &url)),
        Some(b) => {
            let mut c = shlex_split(&b);
            c.push(url.clone());
            c
        }
        None => vec!["xdg-open".to_string(), url.clone()],
    };
    if let Some(program) = cmdline.first() {
        let _ = Command::new(program)
            .args(&cmdline[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    Ok(())
}

/// `tempfile.NamedTemporaryFile("wb", suffix=".html", delete=False)`.
fn tempfile_html() -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    let dir = std::env::temp_dir();
    loop {
        let name: String = (0..8)
            .map(|_| {
                let chars = b"abcdefghijklmnopqrstuvwxyz0123456789_";
                chars[rand::random_range(0..chars.len())] as char
            })
            .collect();
        let path = dir.join(format!("tmp{name}.html"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// `PosixPagingHelpRenderer._send_output_to_pager`.
pub(crate) fn page(output: &[u8]) -> Result<(), String> {
    let pager = std::env::var("MANPAGER")
        .or_else(|_| std::env::var("PAGER"))
        .unwrap_or_else(|_| "less -R".to_string());
    let cmdline = shlex_split(&pager);
    let Some(program) = cmdline.first().filter(|p| exists_on_path(p)) else {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(String::from_utf8_lossy(output).as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
        return Ok(());
    };
    let name = Path::new(program).file_name().and_then(|n| n.to_str());
    let copies_input =
        matches!(name, Some("less" | "more")) || (name == Some("cat") && cmdline.len() == 1);
    if copies_input && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(output);
        let _ = stdout.flush();
        return Ok(());
    }
    let _guard = IgnoreCtrlC::new();
    let mut child = Command::new(program)
        .args(&cmdline[1..])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(output);
    }
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

/// `utils.ignore_ctrl_c`.
struct IgnoreCtrlC(libc::sighandler_t);

impl IgnoreCtrlC {
    fn new() -> IgnoreCtrlC {
        // SAFETY: replacing the SIGINT disposition with SIG_IGN.
        IgnoreCtrlC(unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) })
    }
}

impl Drop for IgnoreCtrlC {
    fn drop(&mut self) {
        // SAFETY: restoring the previous SIGINT disposition.
        unsafe {
            libc::signal(libc::SIGINT, self.0);
        }
    }
}

const BASE_REMOTE_URL: &str = "https://awscli.amazonaws.com/v2/documentation/api";

/// `HelpCommand.url`.
pub(crate) fn url(req: &HelpRequest, has_commands: bool) -> String {
    let base = format!(
        "{BASE_REMOTE_URL}/{}",
        crate::model::REFERENCE_VERSION.trim()
    );
    match req {
        HelpRequest::Provider => format!("{base}/index.html"),
        HelpRequest::TopicLister => format!("{base}/topic/index.html"),
        HelpRequest::Topic(name) => format!("{base}/topic/{name}.html"),
        HelpRequest::Service(name) => format!("{base}/reference/{name}/index.html"),
        HelpRequest::Operation { service, name, .. } => {
            format!("{base}/reference/{service}/{name}.html")
        }
        HelpRequest::Wait(service) => format!("{base}/reference/{service}/wait/index.html"),
        HelpRequest::Waiter { service, name, .. } => {
            format!("{base}/reference/{service}/wait/{name}.html")
        }
        HelpRequest::Custom(path) => {
            if has_commands {
                format!("{base}/reference/{}/index.html", path.join("/"))
            } else {
                format!("{base}/reference/{}.html", path.join("/"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex() {
        assert_eq!(shlex_split("less -R"), vec!["less", "-R"]);
        assert_eq!(
            shlex_split(r#"a "b c" 'd e' f\ g"#),
            vec!["a", "b c", "d e", "f g"]
        );
    }
}
