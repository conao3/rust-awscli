//! Transfer results and their printing (`awscli/customizations/s3/
//! results.py`: the result types, `ResultRecorder`, `ResultPrinter` and
//! its no-progress / only-errors variants, `ResultProcessor`).

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use super::utils::human_readable_size;
use super::{err, out};

#[derive(Debug, Clone)]
pub(crate) struct Loc {
    pub transfer_type: String,
    pub src: String,
    pub dest: Option<String>,
}

impl Loc {
    fn key(&self) -> String {
        let mut parts = vec![self.transfer_type.clone(), self.src.clone()];
        if let Some(d) = &self.dest {
            parts.push(d.clone());
        }
        parts.join(":")
    }

    fn location(&self) -> String {
        match &self.dest {
            None => self.src.clone(),
            Some(d) => format!("{} to {d}", self.src),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum TResult {
    Queued(Loc, Option<i64>),
    Progress(Loc, i64, Option<i64>, f64),
    Success(Loc),
    Failure(Loc, String),
    DryRun(Loc),
    Skip,
    Warning(String),
    Error(String),
    /// `CtrlCResult`: a transfer cancelled by Ctrl-C.
    CtrlC,
    FinalTotal(usize),
}

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Default)]
struct Recorder {
    bytes_transferred: i64,
    bytes_failed_to_transfer: i64,
    files_transferred: i64,
    files_failed: i64,
    files_warned: i64,
    files_skipped: i64,
    errors: i64,
    expected_bytes_transferred: i64,
    expected_files_transferred: i64,
    final_expected_files_transferred: Option<i64>,
    start_time: Option<f64>,
    bytes_transfer_speed: f64,
    ongoing_progress: HashMap<String, i64>,
    ongoing_total_sizes: HashMap<String, Option<i64>>,
}

impl Recorder {
    fn expected_totals_are_final(&self) -> bool {
        self.final_expected_files_transferred == Some(self.expected_files_transferred)
    }

    fn record(&mut self, r: &TResult) {
        match r {
            TResult::Queued(loc, size) => {
                if self.start_time.is_none() {
                    self.start_time = Some(now());
                }
                self.ongoing_total_sizes.insert(loc.key(), *size);
                if let Some(s) = size
                    && *s != 0
                {
                    self.expected_bytes_transferred += s;
                }
                self.expected_files_transferred += 1;
            }
            TResult::Progress(loc, bytes, total, ts) => {
                let key = loc.key();
                if let Some(None) = self.ongoing_total_sizes.get(&key) {
                    match total {
                        Some(t) => {
                            self.ongoing_total_sizes.insert(key.clone(), Some(*t));
                            let progress = self.ongoing_progress.get(&key).copied().unwrap_or(0);
                            self.expected_bytes_transferred += t - progress;
                        }
                        None => self.expected_bytes_transferred += bytes,
                    }
                }
                *self.ongoing_progress.entry(key).or_insert(0) += bytes;
                self.bytes_transferred += bytes;
                if let Some(start) = self.start_time
                    && *ts > start
                {
                    self.bytes_transfer_speed = self.bytes_transferred as f64 / (ts - start);
                }
            }
            TResult::Success(loc) => {
                self.pop(loc);
                self.files_transferred += 1;
            }
            TResult::Failure(loc, _) => {
                let (progress, size) = self.pop(loc);
                if let Some(size) = size {
                    self.bytes_failed_to_transfer += size - progress;
                }
                self.files_failed += 1;
                self.files_transferred += 1;
            }
            TResult::Skip => self.files_skipped += 1,
            TResult::Warning(_) => self.files_warned += 1,
            TResult::Error(_) | TResult::CtrlC => self.errors += 1,
            TResult::FinalTotal(n) => self.final_expected_files_transferred = Some(*n as i64),
            TResult::DryRun(_) => {}
        }
    }

    fn pop(&mut self, loc: &Loc) -> (i64, Option<i64>) {
        let key = loc.key();
        let progress = self.ongoing_progress.remove(&key).unwrap_or(0);
        let size = self.ongoing_total_sizes.remove(&key).flatten();
        (progress, size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Quiet,
    OnlyErrors,
    NoProgress,
    Progress,
}

struct Printer {
    mode: Mode,
    frequency: f64,
    first: bool,
    now: f64,
    oneline: bool,
    progress_length: usize,
}

impl Printer {
    fn pad(&self, s: &str, ending: char) -> String {
        let n = s.chars().count();
        let mut out = s.to_string();
        if n < self.progress_length {
            out.push_str(&" ".repeat(self.progress_length - n));
        }
        out.push(ending);
        out
    }

    fn handle(&mut self, rec: &Recorder, r: &TResult) {
        if self.mode == Mode::Quiet {
            return;
        }
        match r {
            TResult::Progress(..) => self.print_progress(rec),
            TResult::Success(loc) => {
                if self.mode != Mode::OnlyErrors {
                    let s = self.pad(&format!("{}: {}", loc.transfer_type, loc.location()), '\n');
                    out(s.as_bytes());
                    self.redisplay(rec, true);
                }
            }
            TResult::Failure(loc, e) => {
                let s = self.pad(
                    &format!("{} failed: {} {e}", loc.transfer_type, loc.location()),
                    '\n',
                );
                err(s.as_bytes());
                self.redisplay(rec, true);
            }
            TResult::Warning(m) => {
                let s = self.pad(m, '\n');
                err(s.as_bytes());
                self.redisplay(rec, true);
            }
            TResult::Skip => self.redisplay(rec, false),
            TResult::Error(e) => {
                let s = self.pad(&format!("fatal error: {e}"), '\n');
                err(s.as_bytes());
            }
            TResult::CtrlC => {
                let s = self.pad("cancelled: ctrl-c received", '\n');
                err(s.as_bytes());
            }
            TResult::DryRun(loc) => {
                let s = self.pad(
                    &format!("(dryrun) {}: {}", loc.transfer_type, loc.location()),
                    '\n',
                );
                out(s.as_bytes());
            }
            TResult::FinalTotal(_) => self.clear_progress(rec, '\n'),
            TResult::Queued(..) => {}
        }
    }

    fn has_remaining(rec: &Recorder) -> bool {
        if !rec.expected_totals_are_final() {
            return true;
        }
        rec.files_transferred + rec.files_skipped != rec.expected_files_transferred
    }

    fn redisplay(&mut self, rec: &Recorder, reset: bool) {
        if reset {
            self.progress_length = 0;
        }
        if Self::has_remaining(rec) {
            self.print_progress(rec);
        } else {
            self.clear_progress(rec, '\r');
        }
    }

    fn clear_progress(&mut self, rec: &Recorder, ending: char) {
        if self.mode == Mode::Progress && self.progress_length > 0 && !Self::has_remaining(rec) {
            let s = self.pad("", ending);
            out(s.as_bytes());
        }
    }

    fn expected_total(rec: &Recorder, s: String) -> String {
        if rec.expected_totals_are_final() {
            s
        } else {
            format!("~{s}")
        }
    }

    fn print_progress(&mut self, rec: &Recorder) {
        if self.mode != Mode::Progress {
            return;
        }
        let remaining = Self::expected_total(
            rec,
            (rec.expected_files_transferred - (rec.files_transferred + rec.files_skipped))
                .to_string(),
        );
        let mut s = if rec.expected_bytes_transferred > 0 {
            format!(
                "Completed {}/{} ({}/s) with {remaining} file(s) remaining",
                human_readable_size((rec.bytes_transferred + rec.bytes_failed_to_transfer) as f64),
                Self::expected_total(
                    rec,
                    human_readable_size(rec.expected_bytes_transferred as f64)
                ),
                human_readable_size(rec.bytes_transfer_speed),
            )
        } else {
            format!(
                "Completed {} file(s) with {remaining} file(s) remaining",
                rec.files_transferred
            )
        };
        if !rec.expected_totals_are_final() {
            s.push_str(" (calculating...)");
        }
        let line = if self.oneline {
            let l = self.pad(&s, '\r');
            self.progress_length = l.chars().count() - 1;
            l
        } else {
            self.pad(&s, '\n')
        };
        let t = now();
        if self.first || self.frequency == 0.0 || t - self.now >= self.frequency {
            self.now = t;
            self.first = false;
            out(line.as_bytes());
        }
    }
}

/// The result processing thread and the queue feeding it.
pub(crate) struct Processor {
    pub tx: Sender<TResult>,
    handle: JoinHandle<(i64, i64)>,
}

impl Processor {
    pub(crate) fn start(mode: Mode, frequency: i64, oneline: bool) -> Processor {
        let (tx, rx): (Sender<TResult>, Receiver<TResult>) = channel();
        let handle = std::thread::spawn(move || {
            let mut rec = Recorder::default();
            let mut printer = Printer {
                mode,
                frequency: frequency as f64,
                first: true,
                now: now(),
                oneline,
                progress_length: 0,
            };
            let mut enabled = true;
            for r in rx {
                if enabled {
                    rec.record(&r);
                    printer.handle(&rec, &r);
                }
                if matches!(r, TResult::Error(_) | TResult::CtrlC) {
                    enabled = false;
                }
            }
            (rec.files_failed + rec.errors, rec.files_warned)
        });
        Processor { tx, handle }
    }

    /// Shut the thread down: `(num_tasks_failed, num_tasks_warned)`.
    pub(crate) fn finish(self) -> (i64, i64) {
        drop(self.tx);
        self.handle.join().unwrap_or((1, 0))
    }
}
