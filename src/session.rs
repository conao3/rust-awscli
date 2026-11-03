//! The session: configuration, credentials, signing, and sending requests
//! with retries.

use anyhow::Result;

use crate::cli::GlobalArgs;
use crate::config::ConfigLookup;
use crate::endpoint::Endpoint;
use crate::model::{Operation, ServiceModel};
use crate::protocol::{HttpRequest, HttpResponse};

pub struct Session {
    _private: (),
}

/// The result of sending a request: the final response and how many
/// attempts were made.
pub struct SendOutcome {
    pub response: HttpResponse,
    pub attempts: u32,
    /// True when the last attempt was retryable but the attempt limit was
    /// reached (botocore then appends `(reached max retries: N)`).
    pub max_attempts_reached: bool,
}

impl Session {
    pub fn new(globals: &GlobalArgs) -> Result<Session> {
        let _ = globals;
        todo!("Session::new")
    }

    /// The resolved region, if any.
    pub fn region(&self) -> Option<String> {
        todo!("Session::region")
    }

    /// Sign (per the operation's auth types and the endpoint's
    /// `authSchemes`) and send `request` to `endpoint`, retrying per the
    /// configured retry mode.
    pub async fn send(
        &self,
        model: &ServiceModel,
        operation: &Operation<'_>,
        endpoint: &Endpoint,
        request: &HttpRequest,
    ) -> Result<SendOutcome> {
        let _ = (model, operation, endpoint, request);
        todo!("Session::send")
    }
}

impl ConfigLookup for Session {
    fn config_variable(&self, name: &str) -> Option<String> {
        let _ = name;
        todo!("Session::config_variable")
    }
}
