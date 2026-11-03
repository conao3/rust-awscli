//! Argument tables of every operation and waiter command compared with the
//! reference (`fingerprints.rs`).

use sha2::{Digest, Sha256};

use super::arguments::{CommandPath, build_argument_table, fingerprint};
use super::commands::{
    MainCommand, ServiceEntry, is_customized, main_commands, service_commands, waiter_commands,
};
use super::fingerprints::{FINGERPRINTS, MODEL_GAPS};
use crate::model::ServiceModel;

/// The sorted per-command fingerprints of one service.
fn service_fingerprints(cli: &str, service: &str) -> Vec<String> {
    let model = ServiceModel::load(service).expect("model");
    let mut blocks = Vec::new();
    for (name, entry) in service_commands(cli, &model) {
        match entry {
            ServiceEntry::Operation(op) => {
                if is_customized(cli, &name) || MODEL_GAPS.contains(&(cli, name.as_str())) {
                    continue;
                }
                let op = model.operation(&op).expect("operation");
                let (table, _) = build_argument_table(
                    &op,
                    CommandPath {
                        parent: cli,
                        name: &name,
                    },
                );
                blocks.push(format!("## {cli} {name}\n{}", fingerprint(&table)));
            }
            ServiceEntry::Wait => {
                for (wname, waiter) in waiter_commands(&model) {
                    let op_name = model.waiters().expect("waiters")[&waiter]["operation"]
                        .as_str()
                        .expect("waiter operation")
                        .to_string();
                    let op = model.operation(&op_name).expect("operation");
                    let (table, _) = build_argument_table(
                        &op,
                        CommandPath {
                            parent: "wait",
                            name: &wname,
                        },
                    );
                    blocks.push(format!("## {cli} wait {wname}\n{}", fingerprint(&table)));
                }
            }
            ServiceEntry::Custom => {}
        }
    }
    blocks.sort();
    blocks
}

#[test]
fn argument_tables_match_the_reference() {
    let mut failures = Vec::new();
    let mut count = 0;
    for command in main_commands() {
        let MainCommand::Service(cli, service) = command else {
            continue;
        };
        count += 1;
        let digest = hex::encode(Sha256::digest(service_fingerprints(cli, service).concat()));
        match FINGERPRINTS.iter().find(|(n, _)| *n == cli) {
            Some((_, expected)) if *expected == digest => {}
            _ => failures.push(cli),
        }
    }
    assert_eq!(count, FINGERPRINTS.len());
    assert!(failures.is_empty(), "argument tables differ: {failures:?}");
}

/// Writes all fingerprints to `$AWSR_FP_OUT` for diffing against the
/// reference when a service hash differs.
#[test]
#[ignore]
fn dump_fingerprints() {
    let path = std::env::var("AWSR_FP_OUT").expect("AWSR_FP_OUT");
    let mut out = String::new();
    for command in main_commands() {
        if let MainCommand::Service(cli, service) = command {
            out.push_str(&service_fingerprints(cli, service).concat());
        }
    }
    std::fs::write(path, out).expect("write");
}
