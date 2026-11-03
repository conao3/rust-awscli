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

/// `prompt_toolkit.prompt(prompt_string)` followed by `.strip()`. With
/// stdin not a terminal, `prompt_toolkit` warns once on stderr, renders
/// the prompt and the answer on stdout, and ends the line with `\r\n`.
pub fn ptk_prompt(prompt: &str) -> Result<String, PromptError> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let stdin_tty = std::io::stdin().is_terminal();
    if !stdin_tty && !WARNED.swap(true, Ordering::SeqCst) {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(b"Warning: Input is not a terminal (fd=0).\n");
        let _ = err.flush();
    }
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
