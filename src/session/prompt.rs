//! Reading answers from the user: `compat_input` (`input()` after writing
//! the prompt to stdout), `getpass.getpass` (the MFA prompt), and the
//! `prompt_toolkit` prompts of `configure sso` / `login` as they behave
//! when stdin is not a terminal.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// Why a prompt returned no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// `EOFError` from `input()`: `"EOF when reading a line"`.
    InputEof,
    /// `EOFError()` from `prompt_toolkit` (empty message).
    PtkEof,
    /// `KeyboardInterrupt`.
    Interrupted,
}

impl PromptError {
    /// `str(exception)`.
    pub fn message(&self) -> &'static str {
        match self {
            PromptError::InputEof => "EOF when reading a line",
            PromptError::PtkEof | PromptError::Interrupted => "",
        }
    }
}

fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') && !std::io::stdin().is_terminal() {
                    // Text mode keeps a `\r` only on its own; universal
                    // newlines turn `\r\n` into `\n`.
                    line.pop();
                }
            }
            Some(line)
        }
    }
}

fn write_stdout(text: &str) {
    crate::output::stdout::write_flushed(text.as_bytes());
}

/// `compat_input(prompt)`.
pub fn input(prompt: &str) -> Result<String, PromptError> {
    write_stdout(prompt);
    read_line().ok_or(PromptError::InputEof)
}

/// `prompt_toolkit`'s warning, once per process, when stdin is not a
/// terminal.
fn warn_not_terminal(stdin_tty: bool) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !stdin_tty && !WARNED.swap(true, Ordering::SeqCst) {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(b"Warning: Input is not a terminal (fd=0).\n");
        let _ = err.flush();
    }
}

/// `select_menu` (`awscli/customizations/wizard/ui/selectmenu.py`) with
/// stdin not a terminal: the application reads stdin as keys up to an
/// Enter (Down / Up move the selection and wrap, other keys do nothing,
/// Ctrl-C interrupts) and its last frame stays on stdout: up to ten
/// items, the selected one marked with `> `, each padded to the widest
/// item plus the margins. Without an Enter the input ends with `EOFError`
/// after the renderer's closing rows; an interrupt clears the 80-column
/// line.
pub fn select_menu(items: &[String]) -> Result<usize, PromptError> {
    const MAX_HEIGHT: usize = 10;
    let stdin_tty = std::io::stdin().is_terminal();
    warn_not_terminal(stdin_tty);
    let mut input = Vec::new();
    {
        let mut stdin = std::io::stdin().lock();
        loop {
            let buf = match stdin.fill_buf() {
                Ok(b) if !b.is_empty() => b,
                _ => break,
            };
            match buf.iter().position(|b| *b == b'\r' || *b == 0x03) {
                Some(p) => {
                    input.extend_from_slice(&buf[..=p]);
                    stdin.consume(p + 1);
                    break;
                }
                None => {
                    let n = buf.len();
                    input.extend_from_slice(buf);
                    stdin.consume(n);
                }
            }
        }
    }
    let n = items.len();
    let mut selected = 0usize;
    let mut chosen = None;
    let mut interrupted = false;
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with(b"\x1b[B") || rest.starts_with(b"\x1bOB") {
            selected = (selected + 1) % n;
            i += 3;
        } else if rest.starts_with(b"\x1b[A") || rest.starts_with(b"\x1bOA") {
            selected = (selected + n - 1) % n;
            i += 3;
        } else if rest[0] == b'\r' {
            chosen = Some(selected);
            break;
        } else if rest[0] == 0x03 {
            interrupted = true;
            break;
        } else {
            i += 1;
        }
    }
    let visible = n.min(MAX_HEIGHT);
    let first = selected.saturating_sub(visible - 1);
    let width = items
        .iter()
        .map(|t| crate::output::table::get_text_length(t))
        .max()
        .unwrap_or(0);
    let lines: Vec<String> = items[first..first + visible]
        .iter()
        .enumerate()
        .map(|(k, text)| {
            let mark = if first + k == selected { "> " } else { "  " };
            let pad = width - crate::output::table::get_text_length(text) + 3;
            format!("{mark}{text}{}", " ".repeat(pad))
        })
        .collect();
    let mut frame = lines.join("\r\n");
    let result = match (chosen, interrupted) {
        (Some(index), _) => Ok(index),
        (None, true) => {
            let last = frame.rsplit("\r\n").next().unwrap_or_default();
            let fill = 79usize.saturating_sub(crate::output::table::get_text_length(last));
            frame.push_str(&" ".repeat(fill));
            frame.push('\r');
            frame.push_str(&" ".repeat(80));
            frame.push_str("\r\n");
            Err(PromptError::Interrupted)
        }
        (None, false) => {
            if visible < 8 {
                frame.push_str(&"\r\n".repeat(8 - visible));
                frame.push(' ');
            }
            Err(PromptError::PtkEof)
        }
    };
    write_stdout(&frame);
    result
}

/// `prompt_toolkit.prompt(prompt_string)` followed by `.strip()`. With
/// stdin not a terminal, `prompt_toolkit` warns once on stderr, renders
/// the prompt and the answer on stdout, and ends the line with `\r\n`.
pub fn ptk_prompt(prompt: &str) -> Result<String, PromptError> {
    let stdin_tty = std::io::stdin().is_terminal();
    warn_not_terminal(stdin_tty);
    write_stdout(prompt);
    match read_line() {
        Some(line) => {
            if !stdin_tty {
                write_stdout(&format!("{line}\r\n"));
            }
            Ok(line.trim().to_string())
        }
        None => {
            if !stdin_tty {
                write_stdout("\r\n");
            }
            Err(PromptError::PtkEof)
        }
    }
}

/// `getpass.getpass(prompt)`: the prompt goes to the controlling
/// terminal with echo off; without one, a warning and the prompt go to
/// stderr and the answer is read from stdin.
pub fn getpass(prompt: &str) -> Result<String, PromptError> {
    if let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        use std::os::fd::AsRawFd;
        let fd = tty.as_raw_fd();
        let mut old: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut old) } == 0 {
            let mut new = old;
            new.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &new) };
            let _ = tty.write_all(prompt.as_bytes());
            let _ = tty.flush();
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            let mut eof = true;
            while let Ok(1) = tty.read(&mut byte) {
                if byte[0] == b'\n' {
                    eof = false;
                    break;
                }
                buf.push(byte[0]);
            }
            unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &old) };
            let _ = tty.write_all(b"\n");
            let _ = tty.flush();
            if eof && buf.is_empty() {
                return Err(PromptError::InputEof);
            }
            return Ok(String::from_utf8_lossy(&buf).into_owned());
        }
    }
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(
        b"getpass.py:99: GetPassWarning: Can not control echo on the terminal.\nWarning: Password input may be echoed.\n",
    );
    let _ = err.write_all(prompt.as_bytes());
    let _ = err.flush();
    let line = read_line();
    let _ = err.write_all(b"\n");
    let _ = err.flush();
    line.ok_or(PromptError::InputEof)
}
