//! Waiters (`botocore/waiter.py`, `awscli/customizations/waiters.py`).

use anyhow::Result;

use crate::value::Value;

pub enum WaitOutcome {
    Success,
    /// `Waiter <name> failed: <reason>` (botocore `WaiterError`).
    Failed(String),
}

/// One poll: returns the parsed response, or the parsed error response
/// (`{"Error": {"Code": ...}}`) for service errors.
pub type PollFn<'a> = dyn FnMut(&Value) -> Result<Value> + 'a;

pub async fn wait(
    name: &str,
    config: &serde_json::Map<String, serde_json::Value>,
    params: &Value,
    poll: &mut PollFn<'_>,
) -> Result<WaitOutcome> {
    let _ = (name, config, params, poll);
    todo!("waiter::wait")
}
