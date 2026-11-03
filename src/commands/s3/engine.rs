//! The request executor of the classic transfer client
//! (`s3transfer/manager.py` `TransferManager` with `BoundedExecutor`):
//! `max_concurrent_requests` worker threads run request jobs from a queue
//! bounded at `max_queue_size` for new submissions, and a leaky bucket
//! enforces `max_bandwidth` (`s3transfer/bandwidth.py`).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) type Job<'s> = Box<dyn FnOnce(&Pool<'s>) + Send + 's>;

struct State<'s> {
    queue: VecDeque<Job<'s>>,
    closed: bool,
    active: usize,
}

pub(crate) struct Pool<'s> {
    state: Mutex<State<'s>>,
    cv: Condvar,
    max_queue: usize,
}

impl<'s> Pool<'s> {
    pub(crate) fn new(max_queue: usize) -> Pool<'s> {
        Pool {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: false,
                active: 0,
            }),
            cv: Condvar::new(),
            max_queue: max_queue.max(1),
        }
    }

    /// Submit a job from the submitting thread, waiting while the queue is
    /// full.
    pub(crate) fn submit(&self, job: Job<'s>) {
        let mut st = self.state.lock().unwrap();
        while st.queue.len() >= self.max_queue {
            st = self.cv.wait(st).unwrap();
        }
        st.queue.push_back(job);
        self.cv.notify_all();
    }

    /// Queue a follow-up job from inside a running job (never waits).
    pub(crate) fn push(&self, job: Job<'s>) {
        let mut st = self.state.lock().unwrap();
        st.queue.push_back(job);
        self.cv.notify_all();
    }

    /// No more submissions: workers exit once every queued and running
    /// job (and the jobs those queue) is done.
    pub(crate) fn close(&self) {
        let mut st = self.state.lock().unwrap();
        st.closed = true;
        self.cv.notify_all();
    }

    /// Wait until every queued and running job is done.
    pub(crate) fn wait_idle(&self) {
        let mut st = self.state.lock().unwrap();
        while !st.queue.is_empty() || st.active > 0 {
            st = self.cv.wait(st).unwrap();
        }
    }

    pub(crate) fn worker(&self) {
        loop {
            let job = {
                let mut st = self.state.lock().unwrap();
                loop {
                    if let Some(j) = st.queue.pop_front() {
                        st.active += 1;
                        self.cv.notify_all();
                        break j;
                    }
                    if st.closed && st.active == 0 {
                        self.cv.notify_all();
                        return;
                    }
                    st = self.cv.wait(st).unwrap();
                }
            };
            job(self);
            let mut st = self.state.lock().unwrap();
            st.active -= 1;
            self.cv.notify_all();
        }
    }
}

/// `LeakyBucket`: at most `rate` bytes per second across all requests.
pub(crate) struct Limiter {
    rate: f64,
    state: Mutex<(Instant, f64)>,
}

impl Limiter {
    pub(crate) fn new(rate: i64) -> Limiter {
        Limiter {
            rate: rate.max(1) as f64,
            state: Mutex::new((Instant::now(), 0.0)),
        }
    }

    /// Account for `amount` bytes, sleeping as long as the transfer would
    /// otherwise exceed the rate.
    pub(crate) fn consume(&self, amount: usize) {
        let wait = {
            let mut st = self.state.lock().unwrap();
            let now = Instant::now();
            let (start, sent) = *st;
            let sent = sent + amount as f64;
            st.1 = sent;
            let due = start + Duration::from_secs_f64(sent / self.rate);
            due.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn runs_jobs_and_follow_ups() {
        let count = AtomicUsize::new(0);
        let pool = Pool::new(2);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| pool.worker());
            }
            for _ in 0..10 {
                let count = &count;
                pool.submit(Box::new(move |p: &Pool<'_>| {
                    count.fetch_add(1, Ordering::SeqCst);
                    p.push(Box::new(move |_: &Pool<'_>| {
                        count.fetch_add(1, Ordering::SeqCst);
                    }));
                }));
            }
            pool.close();
        });
        assert_eq!(count.load(Ordering::SeqCst), 20);
    }
}
