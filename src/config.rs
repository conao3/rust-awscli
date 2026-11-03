//! Session configuration: global options, environment variables, and the
//! shared config files, resolved the way botocore's session and the AWS CLI
//! do (`botocore/configprovider.py`, `botocore/configloader.py`,
//! `awscli/clidriver.py`).

use std::collections::HashMap;

/// Read access to resolved configuration variables (e.g. `region`,
/// `output`, `cli_binary_format`, `cli_timestamp_format`, `cli_pager`,
/// `retry_mode`, `max_attempts`, `ca_bundle`, `cli_error_format`), in the
/// AWS CLI's precedence (command line, environment, config file, default).
pub trait ConfigLookup {
    fn config_variable(&self, name: &str) -> Option<String>;
}

/// A fixed map, for tests.
impl ConfigLookup for HashMap<String, String> {
    fn config_variable(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}
