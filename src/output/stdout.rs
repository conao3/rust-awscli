//! `sys.stdout` of the reference: a text stream that is line buffered on
//! a terminal and block buffered otherwise, so text written before a
//! child process starts reaches a pipe after the child's own output
//! unless it was flushed (`io.TextIOWrapper` over `io.BufferedWriter`).

use std::io::{IsTerminal as _, Write};
use std::sync::Mutex;

/// `io.DEFAULT_BUFFER_SIZE`.
const BUFFER_SIZE: usize = 8192;

struct Buffered {
    pending: Vec<u8>,
}

impl Buffered {
    const fn new() -> Buffered {
        Buffered {
            pending: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8], terminal: bool, sink: &mut dyn Write) {
        self.pending.extend_from_slice(bytes);
        if terminal || self.pending.len() >= BUFFER_SIZE {
            self.flush(sink);
        }
    }

    fn flush(&mut self, sink: &mut dyn Write) {
        let _ = sink.write_all(&self.pending);
        let _ = sink.flush();
        self.pending.clear();
    }
}

static STDOUT: Mutex<Buffered> = Mutex::new(Buffered::new());

fn with_stdout(f: impl FnOnce(&mut Buffered, &mut dyn Write)) {
    let mut buffered = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut buffered, &mut std::io::stdout().lock());
}

/// `sys.stdout.write(text)`.
pub fn write(bytes: &[u8]) {
    let terminal = std::io::stdout().is_terminal();
    with_stdout(|b, sink| b.write(bytes, terminal, sink));
}

/// `sys.stdout.flush()`.
pub fn flush() {
    with_stdout(|b, sink| b.flush(sink));
}

/// `sys.stdout.write(text)` followed by `sys.stdout.flush()`.
pub fn write_flushed(bytes: &[u8]) {
    with_stdout(|b, sink| {
        b.write(bytes, true, sink);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_holds_text_until_flush() {
        let mut b = Buffered::new();
        let mut sink = Vec::new();
        b.write(b"ssh -i k.pem host\n", false, &mut sink);
        assert!(sink.is_empty());
        b.flush(&mut sink);
        assert_eq!(sink, b"ssh -i k.pem host\n");
    }

    #[test]
    fn pipe_writes_a_full_buffer() {
        let mut b = Buffered::new();
        let mut sink = Vec::new();
        b.write(&[b'a'; BUFFER_SIZE - 1], false, &mut sink);
        assert!(sink.is_empty());
        b.write(b"b", false, &mut sink);
        assert_eq!(sink.len(), BUFFER_SIZE);
        assert!(b.pending.is_empty());
    }

    #[test]
    fn terminal_writes_at_once() {
        let mut b = Buffered::new();
        let mut sink = Vec::new();
        b.write(b"Waiting\n", true, &mut sink);
        assert_eq!(sink, b"Waiting\n");
    }
}
