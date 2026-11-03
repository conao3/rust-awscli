//! Runs one command line end to end.

use crate::error::{CliError, ExitCode};

/// Run the CLI with `args` (without the program name) and return the exit
/// code.
pub fn run(args: Vec<String>) -> i32 {
    match run_inner(args) {
        Ok(code) => code as i32,
        Err(e) => {
            eprint!("{}", e.message);
            e.code as i32
        }
    }
}

fn run_inner(args: Vec<String>) -> Result<ExitCode, CliError> {
    let _ = args;
    Err(CliError::new(ExitCode::General, "not implemented\n"))
}
