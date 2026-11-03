//! The reference CLI's command table (service and operation command
//! names and argument tables after customizations), dumped once by
//! `scripts/cli_table.py` and cached as JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServiceEntry {
    pub cli: String,
    pub operations: HashMap<String, OpEntry>,
}

#[derive(Debug, Deserialize)]
pub struct OpEntry {
    pub cli: String,
    pub args: Vec<Arg>,
    /// Whether the operation accepts `--cli-input-json`.
    pub cli_input_json: bool,
}

/// `[name, serialized member, argument class, required]`.
#[derive(Debug, Deserialize)]
pub struct Arg(pub String, pub Option<String>, pub String, pub bool);

impl Arg {
    pub fn name(&self) -> &str {
        &self.0
    }
    pub fn member(&self) -> Option<&str> {
        self.1.as_deref()
    }
    pub fn class(&self) -> &str {
        &self.2
    }
    pub fn required(&self) -> bool {
        self.3
    }
}

pub type Table = HashMap<String, ServiceEntry>;

pub fn default_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/difftest/cli-table.json")
}

/// Load the table from `path`, generating it with the reference Python
/// sources (`$AWSR_REFERENCE_SRC`) when the file does not exist.
pub fn load(path: &Path) -> Result<Table> {
    if !path.exists() {
        generate(path)?;
    }
    let text = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("parse {}", path.display()))
}

fn generate(path: &Path) -> Result<()> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/cli_table.py");
    eprintln!("generating the CLI command table into {}", path.display());
    let out = Command::new("python3")
        .arg(&script)
        .output()
        .context("run python3 (use the devShell, which sets AWSR_REFERENCE_SRC)")?;
    if !out.status.success() {
        bail!(
            "{} failed:\n{}",
            script.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out.stdout)?;
    Ok(())
}
