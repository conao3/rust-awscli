//! Retries: a port of `botocore/retries/standard.py` (with
//! `quota.py` and `special.py`) and the adaptive client-side rate
//! limiter (`adaptive.py`, `bucket.py`, `throttling.py`). The CLI only
//! accepts the `standard` and `adaptive` modes (`botocore/config.py`
//! `_validate_retry_configuration`).

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::error::SessionError;
use crate::model::Operation;
use crate::protocol::HttpResponse;

pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryMode {
    Standard,
    Adaptive,
}

impl RetryMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            RetryMode::Standard => "standard",
            RetryMode::Adaptive => "adaptive",
        }
    }
}

/// `TransientRetryableChecker._TRANSIENT_ERROR_CODES`.
pub const TRANSIENT_ERROR_CODES: &[&str] = &[
    "RequestTimeout",
    "RequestTimeoutException",
    "PriorRequestNotComplete",
];

/// `TransientRetryableChecker._TRANSIENT_STATUS_CODES`.
pub const TRANSIENT_STATUS_CODES: &[u16] = &[500, 502, 503, 504];

/// `ThrottledRetryableChecker._THROTTLED_ERROR_CODES`.
pub const THROTTLED_ERROR_CODES: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];

/// `ThrottlingStatusCodeChecker._THROTTLING_STATUS_CODES`.
pub const THROTTLING_STATUS_CODES: &[u16] = &[429];

/// `standard._SERVICE_MAX_ATTEMPTS`, keyed by the hyphenized service id.
fn service_max_attempts(service_id: &str) -> Option<u32> {
    matches!(service_id, "dynamodb" | "dynamodb-streams").then_some(4)
}

/// The modeled errors of an operation (`ModeledRetryErrorDetector`):
/// `(error code, retryable)`, where `retryable` is `Some(throttling)` for
/// an error modeled as retryable.
pub fn modeled_errors(operation: &Operation<'_>) -> Vec<(String, Option<bool>)> {
    operation
        .errors()
        .iter()
        .map(|shape| {
            let code = shape
                .def
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .unwrap_or(shape.name)
                .to_string();
            let retryable = shape.def.get("retryable").map(|retryable| {
                retryable
                    .get("throttling")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false)
            });
            (code, retryable)
        })
        .collect()
}

/// What one attempt produced (`RetryContext`).
pub struct AttemptOutcome<'a> {
    /// 1-based.
    pub attempt: u32,
    pub response: Option<&'a HttpResponse>,
    /// `parsed_response['Error']['Code']`.
    pub error_code: Option<&'a str>,
    pub exception: Option<&'a SessionError>,
}

/// The fixed retry conditions of one operation.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// The botocore service name (`special.py` compares it).
    pub service_name: String,
    /// The hyphenized service id.
    pub service_id: String,
    pub operation_name: String,
    pub modeled: Vec<(String, Option<bool>)>,
}

/// The standard retry checks other than the attempt limit
/// (`TransientRetryableChecker` and `ThrottledRetryableChecker`) for a
/// response and its parsed error code.
pub fn is_retryable(response: &HttpResponse, error_code: Option<&str>) -> bool {
    error_code
        .is_some_and(|c| TRANSIENT_ERROR_CODES.contains(&c) || THROTTLED_ERROR_CODES.contains(&c))
        || TRANSIENT_STATUS_CODES.contains(&response.status)
}

impl RetryPolicy {
    /// `ModeledRetryErrorDetector.detect_error_type`: `Some(throttling)`
    /// for an error modeled as retryable.
    fn modeled_type(&self, code: Option<&str>) -> Option<bool> {
        let code = code?;
        self.modeled
            .iter()
            .filter_map(|(c, retryable)| retryable.map(|t| (c, t)))
            .find(|(c, _)| *c == code)
            .map(|(_, throttling)| throttling)
    }

    /// `ModeledRetryErrorDetector.is_modeled_error`.
    fn is_modeled_error(&self, code: Option<&str>) -> bool {
        code.is_some_and(|code| self.modeled.iter().any(|(c, _)| c == code))
    }

    /// `ThrottlingStatusCodeChecker.is_retryable`.
    fn is_throttling_status(&self, o: &AttemptOutcome<'_>) -> bool {
        o.response
            .is_some_and(|r| THROTTLING_STATUS_CODES.contains(&r.status))
            && !self.is_modeled_error(o.error_code)
    }

    /// `ThrottlingErrorDetector.is_throttling_error_from_context`.
    pub fn is_throttling(&self, o: &AttemptOutcome<'_>) -> bool {
        if o.error_code
            .is_some_and(|c| THROTTLED_ERROR_CODES.contains(&c))
        {
            return true;
        }
        match self.modeled_type(o.error_code) {
            Some(throttling) => throttling,
            None => self.is_throttling_status(o),
        }
    }

    /// `StandardRetryConditions.is_retryable` without the attempt limit.
    fn is_retryable_condition(&self, o: &AttemptOutcome<'_>) -> bool {
        let transient = o
            .error_code
            .is_some_and(|c| TRANSIENT_ERROR_CODES.contains(&c))
            || o.response
                .is_some_and(|r| TRANSIENT_STATUS_CODES.contains(&r.status))
            || o.exception.is_some_and(SessionError::is_retryable);
        let throttled = o
            .error_code
            .is_some_and(|c| THROTTLED_ERROR_CODES.contains(&c));
        let throttling_status = self.is_throttling_status(o);
        let modeled = self.modeled_type(o.error_code).is_some();
        // `special.RetryIDPCommunicationError`.
        let idp = self.service_name == "sts" && o.error_code == Some("IDPCommunicationError");
        // `special.RetryDDBChecksumError`.
        let ddb = self.service_name == "dynamodb"
            && o.response.is_some_and(|r| {
                r.header("x-amz-crc32").is_some_and(|expected| {
                    let actual = crc32fast::hash(&r.body) as u64;
                    expected
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .is_none_or(|e| e != actual)
                })
            });
        transient || throttled || throttling_status || modeled || idp || ddb
    }
}

/// The result of `RetryHandler.needs_retry` for one attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryDecision {
    /// `Some(delay)` to retry after sleeping.
    pub delay: Option<Duration>,
    /// `MaxAttemptsReached` was added to the retry metadata.
    pub max_attempts_reached: bool,
    /// `RetryQuotaReached` was added to the retry metadata.
    pub quota_reached: bool,
}

/// `quota.RetryQuota`.
#[derive(Debug)]
struct RetryQuota {
    max: u32,
    available: u32,
}

/// The per-client retry state: the retry quota and, in adaptive mode, the
/// client rate limiter.
#[derive(Debug)]
pub struct RetryState {
    pub mode: RetryMode,
    quota: Mutex<RetryQuota>,
    limiter: Option<Mutex<ClientRateLimiter>>,
}

const RETRY_COST: u32 = 14;
const NO_RETRY_INCREMENT: u32 = 1;
const THROTTLING_RETRY_COST: u32 = 5;

/// `RetryHandler._LONG_POLLING_OPERATIONS`.
fn is_long_polling(service_id: &str, operation: &str) -> bool {
    matches!(
        (service_id, operation),
        ("sqs", "ReceiveMessage")
            | ("sfn", "GetActivityTask")
            | ("swf", "PollForActivityTask" | "PollForDecisionTask")
    )
}

impl RetryState {
    pub fn new(mode: RetryMode) -> RetryState {
        RetryState {
            mode,
            quota: Mutex::new(RetryQuota {
                max: 500,
                available: 500,
            }),
            limiter: (mode == RetryMode::Adaptive).then(|| Mutex::new(ClientRateLimiter::new())),
        }
    }

    /// The attempt limit for an operation (`register_retry_handler`).
    pub fn max_attempts(configured: Option<u32>, service_id: &str) -> u32 {
        configured
            .or_else(|| service_max_attempts(service_id))
            .unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    /// `RetryHandler.needs_retry` (plus the adaptive limiter's
    /// `on_receiving_response`). `quota_capacity` is
    /// `request_context['retry_quota_capacity']`.
    pub fn needs_retry(
        &self,
        policy: &RetryPolicy,
        outcome: &AttemptOutcome<'_>,
        quota_capacity: &mut Option<u32>,
    ) -> RetryDecision {
        if let Some(limiter) = &self.limiter {
            limiter
                .lock()
                .unwrap()
                .on_receiving_response(policy.is_throttling(outcome));
        }
        let under_max = outcome.attempt < policy.max_attempts;
        let mut decision = RetryDecision {
            delay: None,
            max_attempts_reached: !under_max,
            quota_reached: false,
        };
        if !(under_max && policy.is_retryable_condition(outcome)) {
            return decision;
        }
        let cost = if policy.is_throttling(outcome) {
            THROTTLING_RETRY_COST
        } else {
            RETRY_COST
        };
        let acquired = {
            let mut q = self.quota.lock().unwrap();
            if cost > q.available {
                false
            } else {
                q.available -= cost;
                true
            }
        };
        if acquired {
            *quota_capacity = Some(cost);
            decision.delay = Some(self.delay(policy, outcome));
        } else {
            decision.quota_reached = true;
            if is_long_polling(&policy.service_id, &policy.operation_name) {
                std::thread::sleep(self.delay(policy, outcome));
            }
        }
        decision
    }

    /// `ExponentialBackoff.delay_amount`.
    fn delay(&self, policy: &RetryPolicy, outcome: &AttemptOutcome<'_>) -> Duration {
        let r: f64 = rand::random();
        let exp = 2f64.powi(outcome.attempt as i32 - 1);
        let scale = if policy.is_throttling(outcome) {
            1.0
        } else if matches!(policy.service_id.as_str(), "dynamodb" | "dynamodb-streams") {
            0.025
        } else {
            0.05
        };
        let t_i = r * (scale * exp).min(20.0);
        let retry_after = outcome
            .response
            .and_then(|resp| resp.header("x-amz-retry-after"))
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .map(|v| v as f64 / 1000.0);
        let secs = match retry_after {
            Some(ra) => t_i.max(ra.min(5.0 + t_i)),
            None => t_i,
        };
        Duration::from_secs_f64(secs)
    }

    /// `RetryQuotaChecker.release_retry_quota` (`after-call`).
    pub fn release(&self, status: u16, quota_capacity: Option<u32>) {
        if !(200..300).contains(&status) {
            return;
        }
        let amount = quota_capacity.unwrap_or(NO_RETRY_INCREMENT);
        let mut q = self.quota.lock().unwrap();
        if q.max == q.available {
            return;
        }
        q.available += amount.min(q.max - q.available);
    }

    /// The adaptive limiter's `on_sending_request` (`before-send`): the
    /// time to wait before sending.
    pub fn before_send(&self) -> Option<Duration> {
        self.limiter
            .as_ref()
            .and_then(|l| l.lock().unwrap().acquire())
    }

    pub fn available_quota(&self) -> u32 {
        self.quota.lock().unwrap().available
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `bucket.TokenBucket` (non-blocking form: `acquire` returns the time to
/// sleep).
#[derive(Debug)]
struct TokenBucket {
    fill_rate: f64,
    max_capacity: f64,
    current_capacity: f64,
    last_timestamp: Option<f64>,
}

impl TokenBucket {
    const MIN_RATE: f64 = 0.5;

    fn new(max_rate: f64) -> TokenBucket {
        let mut b = TokenBucket {
            fill_rate: 0.0,
            max_capacity: 0.0,
            current_capacity: 0.0,
            last_timestamp: None,
        };
        b.set_max_rate(max_rate);
        b
    }

    fn set_max_rate(&mut self, value: f64) {
        self.refill();
        self.fill_rate = value.max(Self::MIN_RATE);
        self.max_capacity = if value >= 1.0 { value } else { 1.0 };
        self.current_capacity = self.current_capacity.min(self.max_capacity);
    }

    fn refill(&mut self) {
        let t = now_secs();
        let Some(last) = self.last_timestamp else {
            self.last_timestamp = Some(t);
            return;
        };
        let fill = (t - last) * self.fill_rate;
        self.current_capacity = self.max_capacity.min(self.current_capacity + fill);
        self.last_timestamp = Some(t);
    }

    fn acquire(&mut self) -> Option<Duration> {
        self.refill();
        if 1.0 <= self.current_capacity {
            self.current_capacity -= 1.0;
            return None;
        }
        let sleep = (1.0 - self.current_capacity) / self.fill_rate;
        self.current_capacity = 0.0;
        self.last_timestamp = Some(now_secs() + sleep);
        Some(Duration::from_secs_f64(sleep.max(0.0)))
    }
}

/// `throttling.CubicCalculator`.
#[derive(Debug)]
struct CubicCalculator {
    w_max: f64,
    k: f64,
    last_fail: f64,
}

impl CubicCalculator {
    const SCALE_CONSTANT: f64 = 0.4;
    const BETA: f64 = 0.7;

    fn new(starting_max_rate: f64, start_time: f64) -> CubicCalculator {
        let mut c = CubicCalculator {
            w_max: starting_max_rate,
            k: 0.0,
            last_fail: start_time,
        };
        c.k = c.zero_point();
        c
    }

    fn zero_point(&self) -> f64 {
        ((self.w_max * (1.0 - Self::BETA)) / Self::SCALE_CONSTANT).powf(1.0 / 3.0)
    }

    fn success_received(&self, timestamp: f64) -> f64 {
        let dt = timestamp - self.last_fail;
        Self::SCALE_CONSTANT * (dt - self.k).powi(3) + self.w_max
    }

    fn error_received(&mut self, current_rate: f64, timestamp: f64) -> f64 {
        self.w_max = current_rate;
        self.k = self.zero_point();
        self.last_fail = timestamp;
        current_rate * Self::BETA
    }
}

/// `adaptive.RateClocker`.
#[derive(Debug)]
struct RateClocker {
    measured_rate: f64,
    last_bucket: f64,
    count: f64,
}

impl RateClocker {
    const SMOOTHING: f64 = 0.8;
    const TIME_BUCKET_SCALE: f64 = 2.0;

    fn record(&mut self) -> f64 {
        let t = now_secs();
        let bucket = (t * Self::TIME_BUCKET_SCALE).floor() / Self::TIME_BUCKET_SCALE;
        self.count += 1.0;
        if bucket > self.last_bucket {
            let current = self.count / (bucket - self.last_bucket);
            self.measured_rate =
                current * Self::SMOOTHING + self.measured_rate * (1.0 - Self::SMOOTHING);
            self.count = 0.0;
            self.last_bucket = bucket;
        }
        self.measured_rate
    }
}

/// `adaptive.ClientRateLimiter`.
#[derive(Debug)]
struct ClientRateLimiter {
    rate_adjustor: CubicCalculator,
    rate_clocker: RateClocker,
    token_bucket: TokenBucket,
    enabled: bool,
}

impl ClientRateLimiter {
    const MAX_RATE_ADJUST_SCALE: f64 = 2.0;

    fn new() -> ClientRateLimiter {
        let now = now_secs();
        ClientRateLimiter {
            rate_adjustor: CubicCalculator::new(0.0, now),
            rate_clocker: RateClocker {
                measured_rate: 0.0,
                last_bucket: now.floor(),
                count: 0.0,
            },
            token_bucket: TokenBucket::new(1.0),
            enabled: false,
        }
    }

    fn acquire(&mut self) -> Option<Duration> {
        if self.enabled {
            self.token_bucket.acquire()
        } else {
            None
        }
    }

    fn on_receiving_response(&mut self, throttled: bool) {
        let measured = self.rate_clocker.record();
        let timestamp = now_secs();
        let new_rate = if !throttled {
            self.rate_adjustor.success_received(timestamp)
        } else {
            let rate_to_use = if self.enabled {
                measured.min(self.token_bucket.fill_rate)
            } else {
                measured
            };
            self.enabled = true;
            self.rate_adjustor.error_received(rate_to_use, timestamp)
        };
        self.token_bucket
            .set_max_rate(new_rate.min(Self::MAX_RATE_ADJUST_SCALE * measured));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::error::ErrorKind;

    fn policy(max: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts: max,
            service_name: "sts".into(),
            service_id: "sts".into(),
            operation_name: "GetCallerIdentity".into(),
            modeled: vec![
                ("ModeledThing".into(), Some(false)),
                ("ModeledSlow".into(), Some(true)),
                ("ModeledPlain".into(), None),
            ],
        }
    }

    fn resp(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            ..Default::default()
        }
    }

    /// The table of `StandardRetryConditions` (the reference botocore).
    #[test]
    fn classification() {
        let p = policy(3);
        let cases: &[(u16, Option<&str>, bool)] = &[
            (500, None, true),
            (502, None, true),
            (503, None, true),
            (504, None, true),
            (501, None, false),
            (400, Some("Throttling"), true),
            (400, Some("ThrottlingException"), true),
            (400, Some("SlowDown"), true),
            (400, Some("RequestTimeout"), true),
            (400, Some("PriorRequestNotComplete"), true),
            (400, Some("EC2ThrottledException"), true),
            (400, Some("ValidationError"), false),
            (429, None, true),
            (429, Some("UnknownThing"), true),
            (429, Some("ModeledPlain"), false),
            (400, Some("ModeledThing"), true),
            (400, Some("IDPCommunicationError"), true),
            (200, None, false),
        ];
        for (status, code, expected) in cases {
            let r = resp(*status);
            let o = AttemptOutcome {
                attempt: 1,
                response: Some(&r),
                error_code: *code,
                exception: None,
            };
            assert_eq!(p.is_retryable_condition(&o), *expected, "{status} {code:?}");
        }
        for kind in [
            ErrorKind::Connection,
            ErrorKind::ConnectTimeout,
            ErrorKind::ReadTimeout,
            ErrorKind::ConnectionClosed,
            ErrorKind::Ssl,
            ErrorKind::Proxy,
            ErrorKind::HttpClient,
        ] {
            let e = SessionError::new(kind, "x");
            let o = AttemptOutcome {
                attempt: 1,
                response: None,
                error_code: None,
                exception: Some(&e),
            };
            assert!(p.is_retryable_condition(&o));
        }
        let e = SessionError::no_credentials();
        let o = AttemptOutcome {
            attempt: 1,
            response: None,
            error_code: None,
            exception: Some(&e),
        };
        assert!(!p.is_retryable_condition(&o));
        assert!(p.is_throttling(&AttemptOutcome {
            attempt: 1,
            response: None,
            error_code: Some("ModeledSlow"),
            exception: None,
        }));
    }

    #[test]
    fn max_attempts_and_quota() {
        let state = RetryState::new(RetryMode::Standard);
        let p = policy(3);
        let r = resp(500);
        let mut cap = None;
        let mut o = AttemptOutcome {
            attempt: 1,
            response: Some(&r),
            error_code: Some("InternalFailure"),
            exception: None,
        };
        let d = state.needs_retry(&p, &o, &mut cap);
        assert!(d.delay.is_some_and(|d| d <= Duration::from_secs(1)));
        assert!(!d.max_attempts_reached);
        assert_eq!(cap, Some(14));
        assert_eq!(state.available_quota(), 486);
        o.attempt = 3;
        let d = state.needs_retry(&p, &o, &mut cap);
        assert_eq!(d.delay, None);
        assert!(d.max_attempts_reached);
        // A non-retryable error on the last attempt also records
        // MaxAttemptsReached.
        let r400 = resp(400);
        let o = AttemptOutcome {
            attempt: 1,
            response: Some(&r400),
            error_code: Some("ValidationError"),
            exception: None,
        };
        let d = state.needs_retry(&policy(1), &o, &mut None);
        assert!(d.max_attempts_reached);
        state.release(200, Some(14));
        assert_eq!(state.available_quota(), 500);
    }

    #[test]
    fn plain_retryable() {
        assert!(is_retryable(&resp(503), None));
        assert!(is_retryable(&resp(400), Some("Throttling")));
        assert!(!is_retryable(&resp(400), Some("AccessDenied")));
    }
}
